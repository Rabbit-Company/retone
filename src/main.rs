#![allow(clippy::needless_range_loop)]

mod cancel;
mod dolby;
mod edit;
mod hdr10plus;
mod inspect;
mod json;
mod measure;
mod report;
mod video;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hdr10plus::{Hdr10Plus, Severity};
use inspect::clock;
use rabbit_av1::metadata::{ContentLightLevel, MasteringDisplay, PRIMARIES_BT_2020, PRIMARIES_DISPLAY_P3, WHITE_D65};
use report::{Coded, Progress, error_code, event, fail, number};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use video::{Finding, Source};

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RETONE_TARGET"), ")");

#[derive(Parser)]
#[command(
    name = "retone",
    version = VERSION,
    about = "Measure, write, check and remove HDR10+ and HDR10 metadata in AV1 video"
)]
struct Cli {
  #[command(subcommand)]
  command: Command,
  /// How to report progress and results
  #[arg(long, value_enum, default_value = "text", global = true)]
  progress: Progress,
  /// Print nothing except errors, the same as --progress none
  #[arg(short, long, global = true)]
  quiet: bool,
}

#[derive(clap::Args)]
struct VideoArgs {
  /// ffmpeg executable used for containers other than IVF
  #[arg(long, default_value = "ffmpeg")]
  ffmpeg: String,
  /// Frame rate for streams without timestamps, such as OBU files, as 24000/1001 or 25
  #[arg(long, value_parser = parse_fps)]
  fps: Option<(u32, u32)>,
}

impl VideoArgs {
  fn source<'a>(&'a self, path: &'a Path) -> Source<'a> {
    Source {
      path,
      ffmpeg: &self.ffmpeg,
      fps: self.fps,
    }
  }
}

#[derive(Subcommand)]
enum Command {
  /// Show the HDR formats and metadata of an AV1 video
  Info {
    /// AV1 video to describe
    input: PathBuf,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Analyse an HDR10 or HLG video and write HDR10+ metadata for it
  Measure {
    /// Video to measure, in any format ffmpeg decodes
    input: PathBuf,
    /// Output HDR10+ metadata file
    #[arg(short, long)]
    output: PathBuf,
    /// How different two frames must be to start a new scene, from 0 to 1
    #[arg(long, default_value_t = 0.3)]
    scene_threshold: f64,
    /// Give every frame its own metadata instead of one set per scene
    #[arg(long)]
    per_frame: bool,
    /// Peak brightness of the display HLG is shown on, in cd/m²
    #[arg(long, default_value_t = 1000.0)]
    hlg_peak: f64,
    /// Treat the video as PQ or HLG whatever its tags say, for files that lost them
    #[arg(long, value_enum)]
    transfer: Option<measure::Transfer>,
    /// Worker threads, defaults to every available thread
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    /// ffmpeg executable, ffprobe is expected next to it
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
  },
  /// Write HDR10+ metadata for a Dolby Vision video, with the scenes of its Dolby Vision metadata
  Convert {
    /// Dolby Vision video in HEVC or AV1, or an RPU file from dovi_tool
    input: PathBuf,
    /// Output HDR10+ metadata file
    #[arg(short, long)]
    output: PathBuf,
    /// The video to measure, by default the input. Needed for an RPU file or profile 5
    #[arg(long)]
    video: Option<PathBuf>,
    /// How different two frames must be to start a new scene, when the Dolby Vision metadata marks none
    #[arg(long, default_value_t = 0.3)]
    scene_threshold: f64,
    /// Peak brightness of the display HLG is shown on, in cd/m²
    #[arg(long, default_value_t = 1000.0)]
    hlg_peak: f64,
    /// Treat the video as PQ or HLG whatever its tags say, for files that lost them
    #[arg(long, value_enum)]
    transfer: Option<measure::Transfer>,
    /// Worker threads, defaults to every available thread
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    #[command(flatten)]
    video_args: VideoArgs,
  },
  /// Write HDR10+ metadata into an AV1 video
  Apply {
    /// AV1 video to add the metadata to
    input: PathBuf,
    /// Output video
    #[arg(short, long)]
    output: PathBuf,
    /// HDR10+ metadata file, as export, measure and hdr10plus_tool write it
    #[arg(short, long)]
    metadata: PathBuf,
    /// Tag the video as BT.2020 PQ when its colour description says otherwise
    #[arg(long)]
    fix_color: bool,
    /// Read the output back and decode it with dav1d before keeping it
    #[arg(long)]
    verify: bool,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Read the HDR10+ metadata out of an AV1 video
  Export {
    /// AV1 video to read
    input: PathBuf,
    /// Output HDR10+ metadata file
    #[arg(short, long)]
    output: PathBuf,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Remove HDR10+ metadata from an AV1 video
  Remove {
    /// AV1 video to remove metadata from
    input: PathBuf,
    /// Output video
    #[arg(short, long)]
    output: PathBuf,
    /// Also remove Dolby Vision metadata
    #[arg(long)]
    dolby_vision: bool,
    /// Also remove the static HDR10 metadata: mastering display and content light level
    #[arg(long = "static")]
    static_metadata: bool,
    /// Keep HDR10+ metadata, to remove only what the other options name
    #[arg(long)]
    keep_hdr10plus: bool,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Write static HDR10 metadata: mastering display and content light level
  Static {
    /// AV1 video to add the metadata to
    input: PathBuf,
    /// Output video
    #[arg(short, long)]
    output: PathBuf,
    /// Mastering display, as G(x,y)B(x,y)R(x,y)WP(x,y)L(max,min) or as P3 or BT2020 with max and min cd/m², such as p3,1000,0.0001
    #[arg(long)]
    mastering_display: Option<String>,
    /// Brightest pixel of the video in cd/m², MaxCLL
    #[arg(long, requires = "max_fall", conflicts_with = "cll_from")]
    max_cll: Option<u16>,
    /// Highest frame average brightness in cd/m², MaxFALL
    #[arg(long, requires = "max_cll", conflicts_with = "cll_from")]
    max_fall: Option<u16>,
    /// Take MaxCLL and MaxFALL from HDR10+ metadata, a JSON file or a video that carries it
    #[arg(long)]
    cll_from: Option<PathBuf>,
    /// Tag the video as BT.2020 PQ when its colour description says otherwise
    #[arg(long)]
    fix_color: bool,
    /// Overwrite the output file if it exists
    #[arg(short = 'y', long)]
    overwrite: bool,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Show HDR10+ metadata scene by scene
  Stats {
    /// HDR10+ metadata file or AV1 video
    input: PathBuf,
    /// Show every frame instead of every scene
    #[arg(long)]
    frames: bool,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Check an AV1 video or HDR10+ metadata file against the HDR10+ specifications
  Check {
    /// AV1 video or HDR10+ metadata file
    input: PathBuf,
    /// Show every finding instead of the first few of each kind
    #[arg(long)]
    all: bool,
    #[command(flatten)]
    video: VideoArgs,
  },
  /// Print a shell completion script
  Completions {
    /// The shell to complete for
    shell: clap_complete::Shell,
  },
  /// Print the manual page in roff format
  Manual,
}

fn check_output(input: &Path, output: &Path, overwrite: bool) -> Result<()> {
  if output.exists() {
    if let (Ok(a), Ok(b)) = (input.canonicalize(), output.canonicalize())
      && a == b
    {
      return Err(fail("output_is_input", "the output would overwrite the input"));
    }
    if !overwrite {
      return Err(fail(
        "output_exists",
        format!("{} already exists, pass --overwrite to replace it", output.display()),
      ));
    }
  }
  Ok(())
}

// A reader that stops early, such as head, closes the pipe. That is not an error.
fn print_bytes(bytes: &[u8]) -> Result<()> {
  use std::io::Write;
  match std::io::stdout().lock().write_all(bytes) {
    Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error).code("io_error"),
    _ => Ok(()),
  }
}

fn parse_fps(value: &str) -> std::result::Result<(u32, u32), String> {
  let fraction = match value.split_once('/') {
    Some((num, den)) => num.trim().parse::<u32>().ok().zip(den.trim().parse::<u32>().ok()),
    None => value
      .trim()
      .parse::<f64>()
      .ok()
      .filter(|fps| fps.is_finite())
      .map(|fps| ((fps * 1000.0).round() as u32, 1000)),
  };
  fraction
    .filter(|&(num, den)| num > 0 && den > 0)
    .ok_or_else(|| format!("{value} is not a frame rate, use a value like 24000/1001 or 25"))
}

// G(x,y)B(x,y)R(x,y)WP(x,y)L(max,min) as x265 and SVT-AV1 take it, either in their integer
// units (0.00002 and 0.0001 cd/m²) or as plain numbers, or a preset like p3,1000,0.0001.
fn parse_mastering_display(value: &str) -> Result<MasteringDisplay> {
  let invalid = || {
    fail(
      "invalid_argument",
      format!("{value} is not a mastering display, use G(x,y)B(x,y)R(x,y)WP(x,y)L(max,min) or p3,1000,0.0001"),
    )
  };
  let text: String = value
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect::<String>()
    .to_ascii_uppercase();
  let preset = text.split(',').collect::<Vec<_>>();
  if let [name, max, min] = preset[..]
    && !name.contains('(')
  {
    let primaries = match name {
      "P3" | "DISPLAYP3" | "P3D65" => PRIMARIES_DISPLAY_P3,
      "BT2020" | "2020" | "REC2020" => PRIMARIES_BT_2020,
      _ => return Err(invalid()),
    };
    let max: f64 = max.parse().map_err(|_| invalid())?;
    let min: f64 = min.parse().map_err(|_| invalid())?;
    return MasteringDisplay::from_values(primaries, WHITE_D65, max, min).code("invalid_argument");
  }
  // With WP shortened to W, every value pair has a one letter name.
  let text = text.replace("WP(", "W(");
  let pair = |key: char| -> Result<[f64; 2]> {
    let start = text.find(&format!("{key}(")).ok_or_else(invalid)? + 2;
    let end = start + text[start..].find(')').ok_or_else(invalid)?;
    let (a, b) = text[start..end].split_once(',').ok_or_else(invalid)?;
    Ok([a.parse().map_err(|_| invalid())?, b.parse().map_err(|_| invalid())?])
  };
  let (red, green, blue, white, luminance) = (pair('R')?, pair('G')?, pair('B')?, pair('W')?, pair('L')?);
  let integer_units = [red, green, blue, white].iter().flatten().any(|&value| value > 1.0);
  let (xy, nits) = if integer_units { (0.00002, 0.0001) } else { (1.0, 1.0) };
  let scale = |pair: [f64; 2]| [pair[0] * xy, pair[1] * xy];
  MasteringDisplay::from_values(
    [scale(red), scale(green), scale(blue)],
    scale(white),
    luminance[0] * nits,
    luminance[1] * nits,
  )
  .code("invalid_argument")
}

fn path_json(path: &Path) -> Value {
  Value::from(path.to_string_lossy().into_owned())
}

fn is_json(path: &Path) -> bool {
  path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn write_text(path: &Path, text: &str) -> Result<()> {
  std::fs::write(path, text)
    .with_context(|| format!("failed to write {}", path.display()))
    .code("io_error")
}

// The HDR10+ metadata of every frame, from a JSON file or from a video.
fn load_metadata(path: &Path, video: &VideoArgs) -> Result<(Vec<Hdr10Plus>, Option<video::Scan>)> {
  if is_json(path) {
    return Ok((json::read(path)?.frames, None));
  }
  let scan = video::scan(&video.source(path))?;
  if scan.hdr10plus_frames() == 0 {
    return Err(fail("no_metadata", format!("{} has no HDR10+ metadata", path.display())));
  }
  let metadata = scan.metadata()?;
  Ok((metadata, Some(scan)))
}

struct Reporter {
  mode: Progress,
}

impl Reporter {
  fn text(&self, line: impl AsRef<str>) {
    if self.mode == Progress::Text {
      eprintln!("{}", line.as_ref());
    }
  }

  fn json(&self) -> bool {
    self.mode == Progress::Json
  }

  fn summary(&self, command: &str, mut fields: Vec<(&str, Value)>) {
    if self.json() {
      fields.insert(0, ("command", Value::from(command)));
      event("summary", fields);
    }
  }
}

fn mastering_json(mdcv: &MasteringDisplay) -> Value {
  let [r, g, b] = mdcv.primaries_f64();
  let w = mdcv.white_point_f64();
  json!({
    "red": [number(r[0]), number(r[1])],
    "green": [number(g[0]), number(g[1])],
    "blue": [number(b[0]), number(b[1])],
    "white": [number(w[0]), number(w[1])],
    "max_nits": number(mdcv.max_nits()),
    "min_nits": (mdcv.min_nits() * 10_000.0).round() / 10_000.0,
  })
}

fn mastering_text(mdcv: &MasteringDisplay) -> String {
  let primaries = mdcv.primaries_f64();
  let named = if primaries
    == MasteringDisplay::from_values(PRIMARIES_DISPLAY_P3, WHITE_D65, 0.0, 0.0)
      .unwrap()
      .primaries_f64()
  {
    "Display P3".to_string()
  } else if primaries
    == MasteringDisplay::from_values(PRIMARIES_BT_2020, WHITE_D65, 0.0, 0.0)
      .unwrap()
      .primaries_f64()
  {
    "BT.2020".to_string()
  } else {
    let [r, g, b] = primaries;
    format!("R({:.4},{:.4}) G({:.4},{:.4}) B({:.4},{:.4})", r[0], r[1], g[0], g[1], b[0], b[1])
  };
  format!(
    "{named}, {} to {} cd/m²",
    (mdcv.min_nits() * 10_000.0).round() / 10_000.0,
    mdcv.max_nits()
  )
}

fn report_findings(reporter: &Reporter, findings: &[Finding], frame_seconds: &dyn Fn(usize) -> Option<f64>, all: bool) {
  let mut shown: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
  let mut hidden: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
  for finding in findings {
    if reporter.json() {
      event(
        "finding",
        vec![
          ("severity", Value::from(finding.severity.name())),
          ("code", Value::from(finding.code)),
          ("message", Value::from(finding.message.as_str())),
          ("frame", json!(finding.frame)),
          ("temporal_unit", json!(finding.temporal_unit)),
          ("seconds", json!(finding.frame.and_then(frame_seconds).map(number))),
        ],
      );
      continue;
    }
    let count = shown.entry(finding.code).or_default();
    if !all && *count >= 5 {
      *hidden.entry(finding.code).or_default() += 1;
      continue;
    }
    *count += 1;
    let place = match (finding.frame, finding.temporal_unit) {
      (Some(frame), _) => match frame_seconds(frame) {
        Some(seconds) => format!("frame {frame} at {}: ", clock(seconds)),
        None => format!("frame {frame}: "),
      },
      (None, Some(unit)) => format!("temporal unit {unit}: "),
      (None, None) => String::new(),
    };
    reporter.text(format!(
      "{}: {place}{} [{}]",
      finding.severity.name(),
      finding.message,
      finding.code
    ));
  }
  for (code, count) in hidden {
    reporter.text(format!("       and {count} more [{code}], --all shows them"));
  }
}

fn run(command: Command, reporter: &Reporter) -> Result<()> {
  let started = std::time::Instant::now();
  match command {
    Command::Info { input, video } => {
      let scan = video::scan(&video.source(&input))?;
      let sequence = scan
        .sequence
        .clone()
        .context("the video has no sequence header")
        .code("invalid_bitstream")?;
      let formats = inspect::formats(&scan);
      let hdr10plus = scan.metadata().ok();
      let duration = scan.frame_rate().map(|fps| scan.frames.len() as f64 / fps);
      reporter.text(format!("Formats:            {}", formats.join(", ")));
      reporter.text(format!(
        "Video:              {}x{}, {} bit, profile {}, level {}, {} frames{}",
        sequence.max_frame_width,
        sequence.max_frame_height,
        sequence.bit_depth,
        sequence.profile,
        sequence.operating_points.first().map_or(0, |point| point.level),
        scan.frames.len(),
        match (scan.frame_rate(), duration) {
          (Some(fps), Some(seconds)) => format!(", {fps:.3} fps, {}", clock(seconds)),
          _ => String::new(),
        }
      ));
      reporter.text(format!(
        "Colour:             {} primaries, {} transfer, {} matrix, {} range",
        inspect::primaries_name(sequence.color_primaries),
        inspect::transfer_name(sequence.transfer_characteristics),
        inspect::matrix_name(sequence.matrix_coefficients),
        if sequence.full_range { "full" } else { "limited" }
      ));
      match scan.mastering_display.first() {
        Some(mdcv) => reporter.text(format!("Mastering display:  {}", mastering_text(mdcv))),
        None => reporter.text("Mastering display:  none"),
      }
      match scan.content_light.first() {
        Some(cll) => reporter.text(format!(
          "Content light:      MaxCLL {} cd/m², MaxFALL {} cd/m²",
          cll.max_cll, cll.max_fall
        )),
        None => reporter.text("Content light:      none"),
      }
      let with = scan.hdr10plus_frames();
      if with == 0 {
        reporter.text("HDR10+:             none");
      } else {
        let detail = match &hdr10plus {
          Some(metadata) => {
            let cll = edit::content_light_from(metadata);
            format!(
              ", profile {}, {} scenes, peak {} cd/m², highest frame average {} cd/m²",
              metadata
                .first()
                .and_then(Hdr10Plus::profile)
                .map_or("N/A".to_string(), |p| p.to_string()),
              inspect::scenes(metadata).len(),
              cll.max_cll,
              cll.max_fall
            )
          }
          None => String::new(),
        };
        reporter.text(format!("HDR10+:             {with} of {} frames{detail}", scan.frames.len()));
      }
      if scan.dolby_vision > 0 {
        reporter.text(format!("Dolby Vision:       {} metadata OBUs", scan.dolby_vision));
      }
      reporter.text(format!(
        "Film grain:         {}",
        if sequence.film_grain_params_present { "yes" } else { "no" }
      ));
      let errors = scan
        .findings
        .iter()
        .chain(&inspect::stream_findings(&scan))
        .filter(|finding| finding.severity == Severity::Error)
        .count();
      if errors > 0 {
        reporter.text(format!("Problems:           {errors} errors, retone check lists them"));
      }
      reporter.summary(
        "info",
        vec![
          ("formats", json!(formats)),
          ("width", json!(sequence.max_frame_width)),
          ("height", json!(sequence.max_frame_height)),
          ("bit_depth", json!(sequence.bit_depth)),
          ("frames", json!(scan.frames.len())),
          ("frame_rate", json!(scan.frame_rate().map(number))),
          ("color_primaries", json!(sequence.color_primaries)),
          ("transfer_characteristics", json!(sequence.transfer_characteristics)),
          ("matrix_coefficients", json!(sequence.matrix_coefficients)),
          ("full_range", json!(sequence.full_range)),
          (
            "mastering_display",
            scan.mastering_display.first().map_or(Value::Null, mastering_json),
          ),
          (
            "content_light",
            scan
              .content_light
              .first()
              .map_or(Value::Null, |cll| json!({"max_cll": cll.max_cll, "max_fall": cll.max_fall})),
          ),
          ("hdr10plus_frames", json!(with)),
          ("dolby_vision_obus", json!(scan.dolby_vision)),
          ("film_grain", json!(sequence.film_grain_params_present)),
          ("errors", json!(errors)),
        ],
      );
    }
    Command::Measure {
      input,
      output,
      scene_threshold,
      per_frame,
      hlg_peak,
      transfer,
      threads,
      overwrite,
      ffmpeg,
    } => {
      check_output(&input, &output, overwrite)?;
      if !(0.0..=1.0).contains(&scene_threshold) {
        return Err(fail("invalid_argument", "--scene-threshold must be between 0 and 1"));
      }
      if !(100.0..=10_000.0).contains(&hlg_peak) {
        return Err(fail("invalid_argument", "--hlg-peak must be between 100 and 10000"));
      }
      let options = measure::Options {
        ffmpeg: &ffmpeg,
        scene_threshold,
        per_frame,
        hlg_peak,
        transfer,
        scene_starts: None,
        threads: threads
          .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
          .max(1),
        progress: reporter.mode == Progress::Text,
      };
      let result = measure::measure(&input, &options)?;
      if result.hlg {
        reporter.text(format!(
          "The video is HLG. The metadata describes it shown on a {hlg_peak} cd/m² display, as after converting it to PQ. HDR10+ itself needs PQ"
        ));
      }
      write_text(&output, &json::to_json(&result.frames))?;
      let cll = edit::content_light_from(&result.frames);
      reporter.text(format!(
        "Measured {} frames in {} scenes, peak {} cd/m², highest frame average {} cd/m², wrote {}",
        result.frames.len(),
        inspect::scenes(&result.frames).len(),
        cll.max_cll,
        cll.max_fall,
        output.display()
      ));
      reporter.summary(
        "measure",
        vec![
          ("output", path_json(&output)),
          ("frames", json!(result.frames.len())),
          ("scenes", json!(inspect::scenes(&result.frames).len())),
          ("max_cll", json!(cll.max_cll)),
          ("max_fall", json!(cll.max_fall)),
          ("hlg", json!(result.hlg)),
          ("seconds", number(started.elapsed().as_secs_f64())),
        ],
      );
    }
    Command::Convert {
      input,
      output,
      video,
      scene_threshold,
      hlg_peak,
      transfer,
      threads,
      overwrite,
      video_args,
    } => {
      check_output(&input, &output, overwrite)?;
      if !(0.0..=1.0).contains(&scene_threshold) {
        return Err(fail("invalid_argument", "--scene-threshold must be between 0 and 1"));
      }
      let dolby = dolby::read(&input, &video_args.ffmpeg, video_args.fps)?;
      let measured_path = match (&video, dolby.container) {
        (Some(video), _) => video.clone(),
        (None, dolby::Container::RpuFile) => {
          return Err(fail(
            "invalid_argument",
            "an RPU file holds no pictures, pass the video it belongs to with --video",
          ));
        }
        (None, _) if dolby.profile == 5 => {
          return Err(fail(
            "not_hdr",
            "Dolby Vision profile 5 has no HDR10 base layer to measure, pass an HDR10 version of the video with --video",
          ));
        }
        (None, _) => input.clone(),
      };
      let (starts, source) = dolby.scene_starts();
      reporter.text(format!(
        "Read Dolby Vision profile {} metadata of {} frames, {}",
        dolby.profile,
        dolby.frames.len(),
        match (&starts, source) {
          (Some(starts), dolby::CutSource::ShotFlags) => format!("{} shots", starts.iter().filter(|&&start| start).count()),
          (Some(starts), _) => format!("{} shots from its level 1 metadata", starts.iter().filter(|&&start| start).count()),
          (None, _) => "without usable shot boundaries, so scenes are measured".to_string(),
        }
      ));
      let options = measure::Options {
        ffmpeg: &video_args.ffmpeg,
        scene_threshold,
        per_frame: false,
        hlg_peak,
        transfer,
        scene_starts: starts.as_deref(),
        threads: threads
          .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
          .max(1),
        progress: reporter.mode == Progress::Text,
      };
      // Without shot boundaries the frame count is still checked.
      let result = measure::measure(&measured_path, &options)?;
      if result.frames.len() != dolby.frames.len() {
        return Err(fail(
          "frame_count_mismatch",
          format!(
            "the video has {} frames and the Dolby Vision metadata {}",
            result.frames.len(),
            dolby.frames.len()
          ),
        ));
      }
      if result.hlg {
        reporter.text(format!(
          "The video is HLG. The metadata describes it shown on a {hlg_peak} cd/m² display, as after converting it to PQ. HDR10+ itself needs PQ"
        ));
      }
      write_text(&output, &json::to_json(&result.frames))?;
      let cll = edit::content_light_from(&result.frames);
      if let Some(level6) = dolby.level6 {
        reporter.text(format!(
          "Dolby Vision states a {} cd/m² mastering display, MaxCLL {} cd/m² and MaxFALL {} cd/m². Measured: MaxCLL {} cd/m², MaxFALL {} cd/m²",
          level6.max_mastering_nits, level6.max_cll, level6.max_fall, cll.max_cll, cll.max_fall
        ));
      }
      reporter.text(format!(
        "Wrote HDR10+ metadata for {} frames in {} scenes to {}",
        result.frames.len(),
        inspect::scenes(&result.frames).len(),
        output.display()
      ));
      reporter.summary(
        "convert",
        vec![
          ("output", path_json(&output)),
          ("frames", json!(result.frames.len())),
          ("scenes", json!(inspect::scenes(&result.frames).len())),
          ("scene_source", json!(source.name())),
          ("dolby_vision_profile", json!(dolby.profile)),
          (
            "dolby_vision_level6",
            dolby.level6.map_or(Value::Null, |level6| {
              json!({
                "max_mastering_nits": level6.max_mastering_nits,
                "min_mastering_nits": level6.min_mastering as f64 / 10_000.0,
                "max_cll": level6.max_cll,
                "max_fall": level6.max_fall,
              })
            }),
          ),
          ("max_cll", json!(cll.max_cll)),
          ("max_fall", json!(cll.max_fall)),
          ("hlg", json!(result.hlg)),
          ("seconds", number(started.elapsed().as_secs_f64())),
        ],
      );
    }
    Command::Apply {
      input,
      output,
      metadata,
      fix_color,
      verify,
      overwrite,
      video,
    } => {
      check_output(&input, &output, overwrite)?;
      let (frames, _) = load_metadata(&metadata, &video)?;
      let report = edit::apply(&video.source(&input), &output, &frames, &edit::ApplyOptions { fix_color, verify })?;
      if report.color_fixed {
        reporter.text("Tagged the video as BT.2020 PQ");
      }
      if report.replaced > 0 {
        reporter.text(format!("Replaced the HDR10+ metadata of {} frames", report.replaced));
      }
      if report.warnings > 0 {
        reporter.text(format!("The metadata has {} warnings, retone check lists them", report.warnings));
      }
      if !report.has_mastering_display {
        reporter.text("The video has no mastering display metadata, which HDR10+ displays expect. Add it with retone static");
      }
      if report.verified {
        reporter.text("Verified the output: every frame carries its metadata and dav1d decodes it without errors");
      }
      reporter.text(format!(
        "Wrote HDR10+ metadata for {} frames in {} scenes to {}",
        report.frames,
        inspect::scenes(&frames).len(),
        output.display()
      ));
      reporter.summary(
        "apply",
        vec![
          ("output", path_json(&output)),
          ("frames", json!(report.frames)),
          ("replaced_frames", json!(report.replaced)),
          ("color_fixed", json!(report.color_fixed)),
          ("warnings", json!(report.warnings)),
          ("mastering_display", json!(report.has_mastering_display)),
          ("verified", json!(report.verified)),
          ("seconds", number(started.elapsed().as_secs_f64())),
        ],
      );
    }
    Command::Export {
      input,
      output,
      overwrite,
      video,
    } => {
      let to_stdout = output.as_os_str() == "-";
      if !to_stdout {
        check_output(&input, &output, overwrite)?;
      }
      let (frames, _) = load_metadata(&input, &video)?;
      let text = json::to_json(&frames);
      if to_stdout {
        print_bytes(text.as_bytes())?;
      } else {
        write_text(&output, &text)?;
      }
      reporter.text(format!(
        "Exported HDR10+ metadata of {} frames in {} scenes",
        frames.len(),
        inspect::scenes(&frames).len()
      ));
      if !to_stdout {
        reporter.summary(
          "export",
          vec![
            ("output", path_json(&output)),
            ("frames", json!(frames.len())),
            ("scenes", json!(inspect::scenes(&frames).len())),
          ],
        );
      }
    }
    Command::Remove {
      input,
      output,
      dolby_vision,
      static_metadata,
      keep_hdr10plus,
      overwrite,
      video,
    } => {
      check_output(&input, &output, overwrite)?;
      if keep_hdr10plus && !dolby_vision && !static_metadata {
        return Err(fail(
          "invalid_argument",
          "--keep-hdr10plus leaves nothing to remove without --dolby-vision or --static",
        ));
      }
      let options = edit::RemoveOptions {
        hdr10plus: !keep_hdr10plus,
        dolby_vision,
        static_metadata,
      };
      let report = edit::remove(&video.source(&input), &output, options)?;
      let mut parts = Vec::new();
      if options.hdr10plus {
        parts.push(format!("{} HDR10+", report.hdr10plus));
      }
      if dolby_vision {
        parts.push(format!("{} Dolby Vision", report.dolby_vision));
      }
      if static_metadata {
        parts.push(format!("{} static", report.static_metadata));
      }
      reporter.text(format!("Removed {} metadata OBUs, wrote {}", parts.join(", "), output.display()));
      reporter.summary(
        "remove",
        vec![
          ("output", path_json(&output)),
          ("hdr10plus", json!(report.hdr10plus)),
          ("dolby_vision", json!(report.dolby_vision)),
          ("static", json!(report.static_metadata)),
        ],
      );
    }
    Command::Static {
      input,
      output,
      mastering_display,
      max_cll,
      max_fall,
      cll_from,
      fix_color,
      overwrite,
      video,
    } => {
      check_output(&input, &output, overwrite)?;
      let mastering_display = mastering_display.as_deref().map(parse_mastering_display).transpose()?;
      let content_light = match (max_cll, max_fall, &cll_from) {
        (Some(max_cll), Some(max_fall), _) => Some(ContentLightLevel { max_cll, max_fall }),
        (_, _, Some(path)) => Some(edit::content_light_from(&load_metadata(path, &video)?.0)),
        _ => None,
      };
      if mastering_display.is_none() && content_light.is_none() && !fix_color {
        return Err(fail(
          "invalid_argument",
          "nothing to write, pass --mastering-display, --max-cll and --max-fall, --cll-from or --fix-color",
        ));
      }
      let report = edit::write_static(
        &video.source(&input),
        &output,
        &edit::StaticOptions {
          content_light,
          mastering_display,
          fix_color,
        },
      )?;
      if report.color_fixed {
        reporter.text("Tagged the video as BT.2020 PQ");
      }
      if let Some(mdcv) = &mastering_display {
        reporter.text(format!("Mastering display: {}", mastering_text(mdcv)));
      }
      if let Some(cll) = &content_light {
        reporter.text(format!(
          "Content light: MaxCLL {} cd/m², MaxFALL {} cd/m²",
          cll.max_cll, cll.max_fall
        ));
      }
      reporter.text(format!(
        "Wrote the metadata after {} sequence headers to {}",
        report.sequence_headers,
        output.display()
      ));
      reporter.summary(
        "static",
        vec![
          ("output", path_json(&output)),
          ("mastering_display", mastering_display.as_ref().map_or(Value::Null, mastering_json)),
          (
            "content_light",
            content_light.map_or(Value::Null, |cll| json!({"max_cll": cll.max_cll, "max_fall": cll.max_fall})),
          ),
          ("color_fixed", json!(report.color_fixed)),
          ("sequence_headers", json!(report.sequence_headers)),
        ],
      );
    }
    Command::Stats { input, frames, video } => {
      let (metadata, scan) = load_metadata(&input, &video)?;
      let seconds = |frame: usize| scan.as_ref().and_then(|scan| scan.frames.get(frame)).map(|frame| frame.seconds);
      let rows: Vec<(usize, usize)> = if frames {
        (0..metadata.len()).map(|index| (index, 1)).collect()
      } else {
        inspect::scenes(&metadata)
      };
      let label = if frames { "Frame" } else { "Scene" };
      reporter.text(format!(
        "{label:>6} {:>8} {:>7} {:>12}  {:>9} {:>9} {:>9} {:>9}  Target",
        "First", "Frames", "Start", "Peak", "Average", "99.98%", "Y 99.99%"
      ));
      for (row, &(start, length)) in rows.iter().enumerate() {
        let meta = &metadata[start];
        let target = match meta.profile() {
          Some('B') => format!("{} cd/m²", meta.targeted_system_display_maximum_luminance),
          _ => "-".to_string(),
        };
        let reserved = meta.percentile_nits(5).filter(|&value| value > 0.0);
        reporter.text(format!(
          "{row:>6} {start:>8} {length:>7} {:>12}  {:>9.1} {:>9.1} {:>9} {:>9}  {target}",
          seconds(start).map_or("-".to_string(), clock),
          meta.peak_nits(),
          meta.average_nits(),
          meta.percentile_nits(99).map_or("-".to_string(), |value| format!("{value:.1}")),
          reserved.map_or("-".to_string(), |value| format!("{value:.1}")),
        ));
        if reporter.json() {
          event(
            if frames { "frame" } else { "scene" },
            vec![
              ("index", json!(row)),
              ("first_frame", json!(start)),
              ("frames", json!(length)),
              ("seconds", json!(seconds(start).map(number))),
              ("peak_nits", number(meta.peak_nits())),
              ("average_nits", number(meta.average_nits())),
              ("maxscl", json!(meta.main().map(|window| window.maxscl))),
              (
                "distribution",
                json!(meta.main().map(|window| {
                  window
                    .distribution
                    .iter()
                    .map(|percentile| json!({"percentage": percentile.percentage, "value": percentile.value}))
                    .collect::<Vec<_>>()
                })),
              ),
              ("profile", json!(meta.profile().map(|profile| profile.to_string()))),
              ("target_nits", json!(meta.targeted_system_display_maximum_luminance)),
            ],
          );
        }
      }
      let cll = edit::content_light_from(&metadata);
      let scenes = inspect::scenes(&metadata).len();
      reporter.text(format!(
        "{} frames, {scenes} scenes, profile {}, peak {} cd/m², highest frame average {} cd/m²",
        metadata.len(),
        metadata
          .first()
          .and_then(Hdr10Plus::profile)
          .map_or("N/A".to_string(), |p| p.to_string()),
        cll.max_cll,
        cll.max_fall
      ));
      reporter.summary(
        "stats",
        vec![
          ("frames", json!(metadata.len())),
          ("scenes", json!(scenes)),
          ("max_cll", json!(cll.max_cll)),
          ("max_fall", json!(cll.max_fall)),
        ],
      );
    }
    Command::Check { input, all, video } => {
      let (findings, frames, scan) = if is_json(&input) {
        let metadata = json::read(&input)?.frames;
        (inspect::check_metadata(&metadata), metadata.len(), None)
      } else {
        let scan = video::scan(&video.source(&input))?;
        let mut findings = inspect::stream_findings(&scan);
        findings.extend(scan.findings.iter().cloned());
        if scan.hdr10plus_frames() == 0 && !findings.iter().any(|finding| finding.severity == Severity::Error) {
          findings.push(Finding {
            severity: Severity::Warning,
            code: "no_hdr10plus",
            message: "the video has no HDR10+ metadata".to_string(),
            frame: None,
            temporal_unit: None,
          });
        }
        let frames = scan.frames.len();
        (findings, frames, Some(scan))
      };
      let mut findings = findings;
      findings.sort_by_key(|finding| (std::cmp::Reverse(finding.severity), finding.frame, finding.temporal_unit));
      let seconds = |frame: usize| scan.as_ref().and_then(|scan| scan.frames.get(frame)).map(|frame| frame.seconds);
      report_findings(reporter, &findings, &seconds, all);
      let errors = findings.iter().filter(|finding| finding.severity == Severity::Error).count();
      let warnings = findings.len() - errors;
      reporter.summary(
        "check",
        vec![("frames", json!(frames)), ("errors", json!(errors)), ("warnings", json!(warnings))],
      );
      if errors > 0 {
        return Err(fail(
          "check_failed",
          format!("{errors} errors and {warnings} warnings in {frames} frames"),
        ));
      }
      reporter.text(match warnings {
        0 => format!("No problems in {frames} frames"),
        _ => format!("{warnings} warnings in {frames} frames"),
      });
    }
    Command::Completions { shell } => {
      let mut buffer = Vec::new();
      clap_complete::generate(shell, &mut <Cli as clap::CommandFactory>::command(), "retone", &mut buffer);
      print_bytes(&buffer)?;
    }
    Command::Manual => {
      let mut buffer = Vec::new();
      clap_mangen::Man::new(<Cli as clap::CommandFactory>::command())
        .render(&mut buffer)
        .code("io_error")?;
      print_bytes(&buffer)?;
    }
  }
  Ok(())
}

fn main() -> ExitCode {
  let cli = Cli::parse();
  let mode = if cli.quiet { Progress::None } else { cli.progress };
  cancel::install();
  match run(cli.command, &Reporter { mode }) {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      // Ctrl+C also reaches the ffmpeg processes, which then fail before retone
      // notices the signal itself.
      let error = if cancel::requested() {
        fail("cancelled", "cancelled")
      } else {
        error
      };
      if mode == Progress::Json {
        event(
          "error",
          vec![
            ("code", Value::from(error_code(&error))),
            ("message", Value::from(format!("{error:#}"))),
          ],
        );
      } else {
        eprintln!("Error: {error:#}");
      }
      if error_code(&error) == "cancelled" {
        ExitCode::from(130)
      } else {
        ExitCode::FAILURE
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_mastering_display_strings() {
    let svt = parse_mastering_display("G(0.265,0.69)B(0.15,0.06)R(0.68,0.32)WP(0.3127,0.329)L(1000,0.0001)").unwrap();
    let x265 = parse_mastering_display("G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)").unwrap();
    let preset = parse_mastering_display("p3,1000,0.0001").unwrap();
    assert_eq!(svt, preset);
    assert_eq!(x265.max_nits(), 1000.0);
    assert_eq!(x265.primaries, preset.primaries);
    assert!((x265.white_point_f64()[0] - 0.3127).abs() < 1e-4);
    assert!(parse_mastering_display("p4,1000,0").is_err());
    assert!(parse_mastering_display("G(1,2)").is_err());
  }
}
