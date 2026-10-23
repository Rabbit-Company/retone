// Measuring HDR10+ metadata from decoded video. ffmpeg decodes to 10 bit 4:2:0 Y'CbCr, and
// the conversion to light in cd/m² happens here, so it does not depend on ffmpeg's colour code.

use crate::hdr10plus::{APPLICATION_VERSION, Hdr10Plus, PERCENTAGES_9, Percentile, Window};
use crate::report::{Coded, Progress, event, fail, number};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{IsTerminal, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

pub struct Options<'a> {
  pub ffmpeg: &'a str,
  pub scene_threshold: f64,
  pub per_frame: bool,
  pub hlg_peak: f64,
  pub transfer: Option<Transfer>,
  // The frames that start a scene, from other metadata, instead of finding scenes. The video
  // must have exactly these frames.
  pub scene_starts: Option<&'a [bool]>,
  pub threads: usize,
  pub progress: Progress,
  // The command name for the start event, measure or convert.
  pub command: &'a str,
}

pub struct Measured {
  pub frames: Vec<Hdr10Plus>,
  pub hlg: bool,
}

// Signal values are kept with 14 bits, finer than the 10 bit input.
const CODE_BITS: u32 = 14;
const CODES: usize = 1 << CODE_BITS;
// Light is binned by its f32 bit pattern: 10 mantissa bits per octave give steps of 0.1%.
const SHIFT: u32 = 13;
const MIN_NITS: f32 = 1e-4;
const MAX_NITS: f32 = 10_000.0;
const BASE: u32 = MIN_NITS.to_bits() >> SHIFT;
const BINS: usize = ((MAX_NITS.to_bits() >> SHIFT) - BASE + 1) as usize;
// Scenes are compared on a coarse histogram of the brightest component.
const COARSE: usize = 64;

fn bin(nits: f32) -> usize {
  let bits = nits.clamp(MIN_NITS, MAX_NITS).to_bits() >> SHIFT;
  (bits - BASE) as usize
}

fn bin_nits(index: usize) -> f64 {
  if index == 0 {
    return 0.0;
  }
  f32::from_bits((((index as u32) + BASE) << SHIFT) | (1 << (SHIFT - 1))) as f64
}

fn pq_to_nits(signal: f64) -> f64 {
  let (m1, m2) = (2610.0 / 16384.0, 2523.0 / 4096.0 * 128.0);
  let (c1, c2, c3) = (3424.0 / 4096.0, 2413.0 / 4096.0 * 32.0, 2392.0 / 4096.0 * 32.0);
  let p = signal.clamp(0.0, 1.0).powf(1.0 / m2);
  10_000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

// BT.2100 HLG inverse OETF, giving scene light from 0 to 1.
fn hlg_to_scene(signal: f64) -> f64 {
  let (a, b, c) = (0.178_832_77, 0.284_668_92, 0.559_910_73);
  let signal = signal.clamp(0.0, 1.0);
  if signal <= 0.5 {
    signal * signal / 3.0
  } else {
    (((signal - c) / a).exp() + b) / 12.0
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Transfer {
  /// SMPTE ST 2084, as HDR10 uses
  Pq,
  /// ARIB STD-B67 hybrid log-gamma
  Hlg,
}

struct Probe {
  width: usize,
  height: usize,
  transfer: Transfer,
  full_range: bool,
  fps: Option<f64>,
  // The frame count the container states, or its duration times the frame rate.
  frames: Option<u64>,
}

pub fn ffprobe_path(ffmpeg: &str) -> String {
  let path = Path::new(ffmpeg);
  match path.file_name().and_then(|name| name.to_str()) {
    Some(name) if name.contains("ffmpeg") => path
      .with_file_name(name.replace("ffmpeg", "ffprobe"))
      .to_string_lossy()
      .into_owned(),
    _ => "ffprobe".to_string(),
  }
}

fn probe(path: &Path, ffmpeg: &str, forced: Option<Transfer>) -> Result<Probe> {
  let ffprobe = ffprobe_path(ffmpeg);
  let output = Command::new(&ffprobe)
    .args(["-v", "error", "-select_streams", "V:0", "-show_entries"])
    .arg("stream=width,height,color_transfer,color_range,color_space,color_primaries,nb_frames,avg_frame_rate,duration:format=duration")
    .args(["-of", "json"])
    .arg(path)
    .stdin(Stdio::null())
    .output()
    .with_context(|| format!("failed to run {ffprobe}"))
    .code("ffmpeg_missing")?;
  if !output.status.success() {
    return Err(fail(
      "unsupported_input",
      format!(
        "ffprobe could not read {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("")
      ),
    ));
  }
  let value: serde_json::Value = serde_json::from_slice(&output.stdout)
    .context("ffprobe gave no JSON")
    .code("ffmpeg_failed")?;
  let stream = value["streams"]
    .get(0)
    .ok_or_else(|| fail("unsupported_input", format!("{} has no video track", path.display())))?;
  let text = |key: &str| stream[key].as_str().unwrap_or("").to_string();
  let transfer = match (forced, text("color_transfer").as_str()) {
    (Some(transfer), _) => transfer,
    (None, "smpte2084") => Transfer::Pq,
    (None, "arib-std-b67") => Transfer::Hlg,
    (None, other) => {
      return Err(fail(
        "not_hdr",
        format!(
          "the video's transfer is {}, measuring needs PQ (HDR10) or HLG. Pass --transfer when the tags are wrong",
          if other.is_empty() || other == "unknown" {
            "not tagged"
          } else {
            other
          }
        ),
      ));
    }
  };
  match text("color_space").as_str() {
    "bt2020nc" | "" | "unknown" => {}
    other => {
      return Err(fail(
        "unsupported_input",
        format!("the video uses the {other} matrix, measuring needs BT.2020 non-constant luminance"),
      ));
    }
  }
  let width = stream["width"].as_u64().unwrap_or(0) as usize;
  let height = stream["height"].as_u64().unwrap_or(0) as usize;
  if width == 0 || height == 0 {
    return Err(fail("unsupported_input", "ffprobe found no picture size"));
  }
  let fps = text("avg_frame_rate")
    .split_once('/')
    .and_then(|(num, den)| Some(num.parse::<f64>().ok()? / den.parse::<f64>().ok()?))
    .filter(|fps| fps.is_finite() && *fps > 0.0);
  let duration = [&stream["duration"], &value["format"]["duration"]]
    .iter()
    .find_map(|value| value.as_str()?.parse::<f64>().ok())
    .filter(|seconds| seconds.is_finite() && *seconds > 0.0);
  let frames = text("nb_frames")
    .parse::<u64>()
    .ok()
    .filter(|&frames| frames > 0)
    .or_else(|| Some((duration? * fps?).round() as u64));
  Ok(Probe {
    width,
    height,
    transfer,
    full_range: text("color_range") == "pc",
    fps,
    frames,
  })
}

// Lookup tables from 10 bit samples to signal codes, and from codes to light.
struct Tables {
  y: [i32; 1024],
  cr_r: [i32; 1024],
  cb_g: [i32; 1024],
  cr_g: [i32; 1024],
  cb_b: [i32; 1024],
  // Light of a code in cd/m² for PQ, scene light from 0 to 1 for HLG.
  light: Vec<f32>,
  // HLG only: the OOTF gain of a scene luminance from 0 to 1, in steps of 1/65535.
  gain: Vec<f32>,
  hlg_peak: f32,
}

impl Tables {
  fn new(probe: &Probe, hlg_peak: f64) -> Tables {
    let scale = (CODES - 1) as f64;
    let (kr, kb) = (0.2627, 0.0593);
    let kg = 1.0 - kr - kb;
    let luma = |code: usize| {
      if probe.full_range {
        code as f64 / 1023.0
      } else {
        (code as f64 - 64.0) / 876.0
      }
    };
    let chroma = |code: usize| {
      if probe.full_range {
        (code as f64 - 512.0) / 1023.0
      } else {
        (code as f64 - 512.0) / 896.0
      }
    };
    let fixed = |value: f64| (value * scale).round() as i32;
    let mut tables = Tables {
      y: [0; 1024],
      cr_r: [0; 1024],
      cb_g: [0; 1024],
      cr_g: [0; 1024],
      cb_b: [0; 1024],
      light: Vec::new(),
      gain: Vec::new(),
      hlg_peak: hlg_peak as f32,
    };
    for code in 0..1024 {
      tables.y[code] = fixed(luma(code));
      tables.cr_r[code] = fixed(2.0 * (1.0 - kr) * chroma(code));
      tables.cb_b[code] = fixed(2.0 * (1.0 - kb) * chroma(code));
      tables.cb_g[code] = fixed(-2.0 * kb * (1.0 - kb) / kg * chroma(code));
      tables.cr_g[code] = fixed(-2.0 * kr * (1.0 - kr) / kg * chroma(code));
    }
    tables.light = (0..CODES)
      .map(|code| {
        let signal = code as f64 / scale;
        match probe.transfer {
          Transfer::Pq => pq_to_nits(signal) as f32,
          Transfer::Hlg => hlg_to_scene(signal) as f32,
        }
      })
      .collect();
    if probe.transfer == Transfer::Hlg {
      // BT.2100: system gamma 1.2 at 1000 cd/m², adjusted for other display peaks.
      let gamma = 1.2 + 0.42 * (hlg_peak / 1000.0).log10();
      tables.gain = (0..=65535)
        .map(|step| (hlg_peak * (step as f64 / 65535.0).powf(gamma - 1.0)) as f32)
        .collect();
    }
    tables
  }
}

#[derive(Clone)]
struct Stats {
  brightest: Vec<u32>,
  luminance: Vec<u32>,
  max: [f32; 3],
  sum: f64,
  pixels: u64,
}

impl Stats {
  fn new() -> Stats {
    Stats {
      brightest: vec![0; BINS],
      luminance: vec![0; BINS],
      max: [0.0; 3],
      sum: 0.0,
      pixels: 0,
    }
  }

  fn clear(&mut self) {
    self.brightest.fill(0);
    self.luminance.fill(0);
    self.max = [0.0; 3];
    self.sum = 0.0;
    self.pixels = 0;
  }

  fn add(&mut self, other: &Stats) {
    for (a, b) in self.brightest.iter_mut().zip(&other.brightest) {
      *a += b;
    }
    for (a, b) in self.luminance.iter_mut().zip(&other.luminance) {
      *a += b;
    }
    for c in 0..3 {
      self.max[c] = self.max[c].max(other.max[c]);
    }
    self.sum += other.sum;
    self.pixels += other.pixels;
  }

  fn percentile(histogram: &[u32], pixels: u64, percent: f64) -> f64 {
    let target = (pixels as f64 * percent / 100.0).ceil().max(1.0) as u64;
    let mut seen = 0u64;
    for (index, &count) in histogram.iter().enumerate() {
      seen += count as u64;
      if seen >= target {
        return bin_nits(index);
      }
    }
    bin_nits(histogram.len() - 1)
  }

  fn coarse(&self) -> [f64; COARSE] {
    let mut out = [0.0; COARSE];
    let total = self.pixels.max(1) as f64;
    for (index, &count) in self.brightest.iter().enumerate() {
      out[index * COARSE / BINS] += count as f64 / total;
    }
    out
  }

  fn values(&self) -> FrameValues {
    let pixels = self.pixels.max(1);
    let percentile = |percent: f64| Self::percentile(&self.brightest, pixels, percent);
    let dim = self.luminance[..=bin(100.0)].iter().map(|&count| count as u64).sum::<u64>();
    FrameValues {
      max: self.max.map(f64::from),
      average: self.sum / pixels as f64,
      percentiles: PERCENTILES.map(percentile),
      top: percentile(99.98),
      luminance_top: Self::percentile(&self.luminance, pixels, 99.99),
      dim: dim as f64 / pixels as f64,
    }
  }
}

// The brightest component percentiles that go into the distribution, besides 99.98%.
const PERCENTILES: [f64; 6] = [1.0, 25.0, 50.0, 75.0, 90.0, 95.0];

// What one frame contributes to the metadata of its scene, in cd/m².
#[derive(Clone, Copy, Debug)]
struct FrameValues {
  max: [f64; 3],
  average: f64,
  percentiles: [f64; 6],
  top: f64,
  // The 99.99th percentile of luminance.
  luminance_top: f64,
  // The share of pixels with a luminance of at most 100 cd/m².
  dim: f64,
}

// The metadata of a scene, combined from its frames the way the HDR10+ metadata of Netflix's
// conformance streams is: peaks are the highest of any frame, averages and percentiles are
// averaged over the frames. HDR10+ values are in 0.1 cd/m².
fn scene_metadata(frames: &[FrameValues]) -> Hdr10Plus {
  let count = frames.len().max(1) as f64;
  let mean = |get: &dyn Fn(&FrameValues) -> f64| frames.iter().map(get).sum::<f64>() / count;
  let highest = |get: &dyn Fn(&FrameValues) -> f64| frames.iter().map(get).fold(0.0, f64::max);
  let value = |nits: f64| ((nits * 10.0).round() as u32).min(crate::hdr10plus::MAX_VALUE);
  let maxscl = [0, 1, 2].map(|c| value(highest(&|frame| frame.max[c])));
  // A histogram bin stands for its middle, which can be a little above the brightest
  // component. No value may be.
  let peak = maxscl.iter().copied().max().unwrap_or(0);
  let p = |index: usize| value(mean(&|frame| frame.percentiles[index])).min(peak);
  let values = [
    p(0),
    // ST 2094-40 reserves the 5% and 10% entries. HDR10+ puts the 99.99th percentile of
    // luminance and the percentage of pixels up to 100 cd/m² there.
    value(highest(&|frame| frame.luminance_top)).min(peak),
    (mean(&|frame| frame.dim) * 100.0).round() as u32,
    p(1),
    p(2),
    p(3),
    p(4),
    p(5),
    value(highest(&|frame| frame.top)).min(peak),
  ];
  Hdr10Plus {
    application_version: APPLICATION_VERSION,
    targeted_system_display_maximum_luminance: 0,
    targeted_system_display_actual_peak_luminance: None,
    windows: vec![Window {
      area: None,
      maxscl,
      average_maxrgb: value(mean(&|frame| frame.average)).min(peak),
      distribution: PERCENTAGES_9
        .iter()
        .zip(values)
        .map(|(&percentage, value)| Percentile { percentage, value })
        .collect(),
      fraction_bright_pixels: 0,
      bezier_curve: None,
      color_saturation_weight: None,
    }],
    mastering_display_actual_peak_luminance: None,
  }
}

struct Picture<'a> {
  width: usize,
  height: usize,
  y: &'a [u8],
  u: &'a [u8],
  v: &'a [u8],
}

fn sample(plane: &[u8], index: usize) -> usize {
  (u16::from_le_bytes([plane[2 * index], plane[2 * index + 1]]) & 1023) as usize
}

fn analyse_rows(picture: &Picture, tables: &Tables, rows: std::ops::Range<usize>, stats: &mut Stats) {
  let top = (CODES - 1) as i32;
  let chroma_width = picture.width.div_ceil(2);
  for row in rows {
    let mut sum = 0.0f64;
    for x in 0..picture.width {
      let chroma = (row / 2) * chroma_width + x / 2;
      let (cb, cr) = (sample(picture.u, chroma), sample(picture.v, chroma));
      let y = tables.y[sample(picture.y, row * picture.width + x)];
      let r = (y + tables.cr_r[cr]).clamp(0, top) as usize;
      let g = (y + tables.cb_g[cb] + tables.cr_g[cr]).clamp(0, top) as usize;
      let b = (y + tables.cb_b[cb]).clamp(0, top) as usize;
      let mut light = [tables.light[r], tables.light[g], tables.light[b]];
      if !tables.gain.is_empty() {
        let scene = 0.2627 * light[0] + 0.678 * light[1] + 0.0593 * light[2];
        let gain = tables.gain[(scene.clamp(0.0, 1.0) * 65535.0) as usize];
        light = light.map(|value| value * gain);
      }
      let brightest = light[0].max(light[1]).max(light[2]);
      let luminance = 0.2627 * light[0] + 0.678 * light[1] + 0.0593 * light[2];
      stats.brightest[bin(brightest)] += 1;
      stats.luminance[bin(luminance)] += 1;
      for c in 0..3 {
        stats.max[c] = stats.max[c].max(light[c]);
      }
      sum += brightest as f64;
    }
    stats.sum += sum;
    stats.pixels += picture.width as u64;
  }
  let _ = tables.hlg_peak;
}

fn analyse(picture: &Picture, tables: &Tables, workers: &mut [Stats], frame: &mut Stats) {
  frame.clear();
  let per_worker = picture.height.div_ceil(workers.len());
  std::thread::scope(|scope| {
    for (index, stats) in workers.iter_mut().enumerate() {
      let start = (index * per_worker).min(picture.height);
      let end = ((index + 1) * per_worker).min(picture.height);
      scope.spawn(move || {
        stats.clear();
        analyse_rows(picture, tables, start..end, stats);
      });
    }
  });
  for stats in workers.iter() {
    frame.add(stats);
  }
}

// Reports frames done, as regrain does: a JSON progress event, or a line that overwrites itself on a terminal.
fn report_progress(mode: Progress, done: u64, expected: Option<u64>, seconds: f64) {
  let rate = if seconds > 0.0 { done as f64 / seconds } else { 0.0 };
  let total = expected.filter(|&total| total > 0).map(|total| total.max(done));
  let remaining = total
    .filter(|_| rate > 0.0)
    .map(|total| ((total - done) as f64 / rate).round() as u64);
  match mode {
    Progress::Json => event(
      "progress",
      vec![
        ("frames", json!(done)),
        ("total", json!(total)),
        ("percent", json!(total.map(|total| number(done as f64 * 100.0 / total as f64)))),
        ("fps", number(rate)),
        ("elapsed_seconds", number(seconds)),
        ("eta_seconds", json!(remaining)),
      ],
    ),
    Progress::Text => match (total, remaining) {
      (Some(total), Some(remaining)) => eprint!(
        "\rMeasured {done}/{total} frames ({}%), {rate:.1} fps, eta {}:{:02}   ",
        done * 100 / total,
        remaining / 60,
        remaining % 60
      ),
      _ => eprint!("\rMeasured {done} frames, {rate:.1} fps   "),
    },
    Progress::None => {}
  }
}

// Half the summed difference of two coarse histograms, from 0 for the same to 1 for no overlap.
fn distance(a: &[f64; COARSE], b: &[f64; COARSE]) -> f64 {
  a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f64>() / 2.0
}

struct Decoder {
  child: Child,
}

impl Drop for Decoder {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

pub fn measure(input: &Path, options: &Options) -> Result<Measured> {
  if !input.is_file() {
    return Err(fail("input_not_found", format!("input file not found: {}", input.display())));
  }
  let probe = probe(input, options.ffmpeg, options.transfer)?;
  let tables = Tables::new(&probe, options.hlg_peak);
  let (width, height) = (probe.width, probe.height);
  let chroma = width.div_ceil(2) * height.div_ceil(2);
  let frame_bytes = 2 * (width * height + 2 * chroma);
  let mut child = Command::new(options.ffmpeg)
    .args(["-v", "error", "-nostdin", "-i"])
    .arg(input)
    .args([
      "-map",
      "0:V:0",
      "-an",
      "-sn",
      "-dn",
      "-f",
      "rawvideo",
      "-pix_fmt",
      "yuv420p10le",
      "-",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()
    .with_context(|| format!("failed to run {}", options.ffmpeg))
    .code("ffmpeg_missing")?;
  let mut stdout = child.stdout.take().unwrap();
  let mut decoder = Decoder { child };

  // A reader thread keeps ffmpeg decoding while frames are analysed.
  let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(2);
  let (recycle_sender, recycle) = mpsc::channel::<Vec<u8>>();
  let reader = std::thread::spawn(move || -> std::io::Result<()> {
    loop {
      let mut buffer = recycle.try_recv().unwrap_or_else(|_| vec![0u8; frame_bytes]);
      let mut filled = 0;
      while filled < frame_bytes {
        match stdout.read(&mut buffer[filled..])? {
          0 if filled == 0 => return Ok(()),
          0 => {
            return Err(std::io::Error::new(
              std::io::ErrorKind::UnexpectedEof,
              "ffmpeg stopped inside a frame",
            ));
          }
          read => filled += read,
        }
      }
      if sender.send(buffer).is_err() {
        return Ok(());
      }
    }
  });

  let threads = options.threads.min(height).max(1);
  let mut workers = vec![Stats::new(); threads];
  let mut frame = Stats::new();
  let mut scene: Vec<FrameValues> = Vec::new();
  let mut previous: Option<[f64; COARSE]> = None;
  let mut frames: Vec<Hdr10Plus> = Vec::new();
  let started = std::time::Instant::now();
  let mut last_report = started;
  let expected = options.scene_starts.map(|starts| starts.len() as u64).or(probe.frames);
  let terminal = options.progress == Progress::Text && std::io::stderr().is_terminal();
  if options.progress == Progress::Json {
    event(
      "start",
      vec![
        ("command", Value::from(options.command)),
        ("frames", json!(expected)),
        ("width", json!(width)),
        ("height", json!(height)),
        ("fps", json!(probe.fps.map(number))),
        ("transfer", Value::from(if probe.transfer == Transfer::Hlg { "hlg" } else { "pq" })),
      ],
    );
  }
  let flush = |scene: &mut Vec<FrameValues>, frames: &mut Vec<Hdr10Plus>| {
    if !scene.is_empty() {
      frames.extend(std::iter::repeat_n(scene_metadata(scene), scene.len()));
      scene.clear();
    }
  };
  for buffer in receiver.iter() {
    crate::cancel::check()?;
    let picture = Picture {
      width,
      height,
      y: &buffer[..2 * width * height],
      u: &buffer[2 * width * height..2 * (width * height + chroma)],
      v: &buffer[2 * (width * height + chroma)..],
    };
    analyse(&picture, &tables, &mut workers, &mut frame);
    let _ = recycle_sender.send(buffer);
    let coarse = frame.coarse();
    let index = frames.len() + scene.len();
    let cut = match options.scene_starts {
      Some(starts) => *starts.get(index).ok_or_else(|| {
        fail(
          "frame_count_mismatch",
          format!("the video has more frames than the metadata's {}", starts.len()),
        )
      })?,
      None => options.per_frame || previous.is_some_and(|previous| distance(&previous, &coarse) > options.scene_threshold),
    };
    if cut {
      flush(&mut scene, &mut frames);
    }
    previous = Some(coarse);
    scene.push(frame.values());
    if (terminal || options.progress == Progress::Json) && last_report.elapsed().as_millis() >= 500 {
      last_report = std::time::Instant::now();
      report_progress(
        options.progress,
        (frames.len() + scene.len()) as u64,
        expected,
        started.elapsed().as_secs_f64(),
      );
    }
  }
  flush(&mut scene, &mut frames);
  if terminal {
    eprint!("\r\x1b[K");
  }
  let read = reader
    .join()
    .unwrap_or_else(|_| Err(std::io::Error::other("the reader thread failed")));
  crate::cancel::check()?;
  let status = decoder.child.wait().context("failed to wait for ffmpeg").code("ffmpeg_failed")?;
  if !status.success() {
    return Err(fail(
      "ffmpeg_failed",
      format!("ffmpeg failed while decoding {} ({status})", input.display()),
    ));
  }
  read.context("failed to read the decoded video").code("ffmpeg_failed")?;
  if frames.is_empty() {
    return Err(fail(
      "unsupported_input",
      format!("ffmpeg decoded no frames from {}", input.display()),
    ));
  }
  if let Some(starts) = options.scene_starts
    && starts.len() != frames.len()
  {
    return Err(fail(
      "frame_count_mismatch",
      format!("the video has {} frames and the metadata {}", frames.len(), starts.len()),
    ));
  }
  Ok(Measured {
    frames,
    hlg: probe.transfer == Transfer::Hlg,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pq_and_hlg_curves() {
    assert!((pq_to_nits(1.0) - 10_000.0).abs() < 0.01);
    assert!((pq_to_nits(0.508_078) - 100.0).abs() < 0.1);
    assert_eq!(pq_to_nits(0.0), 0.0);
    assert!((hlg_to_scene(0.5) - 1.0 / 12.0).abs() < 1e-9);
    assert!((hlg_to_scene(1.0) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn bins_cover_the_range_finely() {
    assert_eq!(bin(0.0), 0);
    assert_eq!(bin(MAX_NITS), BINS - 1);
    for nits in [0.005, 0.5, 100.0, 999.0, 4000.0] {
      let back = bin_nits(bin(nits));
      assert!((back - nits as f64).abs() / (nits as f64) < 0.0011, "{nits} {back}");
    }
  }

  fn grey_frame(width: usize, height: usize, luma: u16) -> Vec<u8> {
    let chroma = width.div_ceil(2) * height.div_ceil(2);
    let mut data = Vec::new();
    for _ in 0..width * height {
      data.extend(luma.to_le_bytes());
    }
    for _ in 0..2 * chroma {
      data.extend(512u16.to_le_bytes());
    }
    data
  }

  #[test]
  fn measures_a_grey_picture() {
    let probe = Probe {
      width: 6,
      height: 4,
      transfer: Transfer::Pq,
      full_range: false,
      fps: None,
      frames: None,
    };
    let tables = Tables::new(&probe, 1000.0);
    // 10 bit limited range code 509 is PQ 0.508, about 100 cd/m².
    let data = grey_frame(6, 4, 509);
    let picture = Picture {
      width: 6,
      height: 4,
      y: &data[..48],
      u: &data[48..60],
      v: &data[60..],
    };
    let mut workers = vec![Stats::new(); 3];
    let mut frame = Stats::new();
    analyse(&picture, &tables, &mut workers, &mut frame);
    assert_eq!(frame.pixels, 24);
    let meta = scene_metadata(&[frame.values()]);
    let window = &meta.windows[0];
    for value in window.maxscl {
      assert!((value as i64 - 1000).abs() <= 3, "{value}");
    }
    assert!((window.average_maxrgb as i64 - 1000).abs() <= 3);
    assert!(meta.validate().is_empty(), "{:?}", meta.validate());
  }
}
