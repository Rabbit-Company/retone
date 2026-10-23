// Reading Dolby Vision metadata (RPUs) frame by frame, in display order, from HEVC, AV1 or an
// RPU file as dovi_tool writes it.

use crate::report::{fail, Coded};
use crate::video::{explain, Source};
use anyhow::{Context, Result};
use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlock;
use hevc_parser::hevc::NAL_UNSPEC62;
use hevc_parser::HevcParser;
use rabbit_av1::metadata::Metadata;
use rabbit_av1::{Input, ObuKind, StreamParser};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
  RpuFile,
  Hevc,
  Av1,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
  pub scene_cut: bool,
  // Level 1: minimum, maximum and average as 12 bit PQ codes.
  pub level1: Option<[u16; 3]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level6 {
  pub max_mastering_nits: u16,
  // In units of 0.0001 cd/m².
  pub min_mastering: u16,
  pub max_cll: u16,
  pub max_fall: u16,
}

pub struct DolbyVision {
  pub container: Container,
  pub profile: u8,
  pub frames: Vec<Frame>,
  pub level6: Option<Level6>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutSource {
  ShotFlags,
  Level1,
  None,
}

impl CutSource {
  pub fn name(self) -> &'static str {
    match self {
      CutSource::ShotFlags => "dolby_vision_shots",
      CutSource::Level1 => "dolby_vision_level1",
      CutSource::None => "measured",
    }
  }
}

impl DolbyVision {
  // Where scenes start. Dolby Vision marks the first frame of every shot. Some tools mark every
  // frame or none, and then the shots show as changes of the level 1 metadata, which Dolby
  // Vision keeps constant within a shot. When neither works, None leaves it to measuring.
  pub fn scene_starts(&self) -> (Option<Vec<bool>>, CutSource) {
    let count = self.frames.len();
    let flagged = self.frames.iter().skip(1).filter(|frame| frame.scene_cut).count();
    if flagged > 0 && flagged < count.saturating_sub(1) {
      let starts = self
        .frames
        .iter()
        .enumerate()
        .map(|(index, frame)| index == 0 || frame.scene_cut)
        .collect();
      return (Some(starts), CutSource::ShotFlags);
    }
    let changes: Vec<bool> = self
      .frames
      .iter()
      .enumerate()
      .map(|(index, frame)| index == 0 || frame.level1 != self.frames[index - 1].level1)
      .collect();
    let changed = changes.iter().skip(1).filter(|&&change| change).count();
    // Shots last more than two frames on average, a level 1 that changes more often is analysed per frame.
    if self.frames.iter().all(|frame| frame.level1.is_some()) && changed > 0 && changed * 2 < count {
      return (Some(changes), CutSource::Level1);
    }
    (None, CutSource::None)
  }
}

fn frame_of(rpu: &DoviRpu) -> Frame {
  let Some(dm) = rpu.vdr_dm_data.as_ref() else {
    return Frame::default();
  };
  let level1 = match dm.get_block(1) {
    Some(ExtMetadataBlock::Level1(block)) => Some([block.min_pq, block.max_pq, block.avg_pq]),
    _ => None,
  };
  Frame {
    scene_cut: dm.scene_refresh_flag == 1,
    level1,
  }
}

fn level6_of(rpu: &DoviRpu) -> Option<Level6> {
  match rpu.vdr_dm_data.as_ref()?.get_block(6) {
    Some(ExtMetadataBlock::Level6(block)) => Some(Level6 {
      max_mastering_nits: block.max_display_mastering_luminance,
      min_mastering: block.min_display_mastering_luminance,
      max_cll: block.max_content_light_level,
      max_fall: block.max_frame_average_light_level,
    }),
    _ => None,
  }
}

fn ffprobe_codec(path: &Path, ffmpeg: &str) -> Result<String> {
  let ffprobe = crate::measure::ffprobe_path(ffmpeg);
  let output = Command::new(&ffprobe)
    .args([
      "-v",
      "error",
      "-select_streams",
      "V:0",
      "-show_entries",
      "stream=codec_name",
      "-of",
      "default=nw=1:nk=1",
    ])
    .arg(path)
    .stdin(Stdio::null())
    .output()
    .with_context(|| format!("failed to run {ffprobe}"))
    .code("ffmpeg_missing")?;
  let text = String::from_utf8_lossy(&output.stdout);
  Ok(text.lines().next().unwrap_or("").trim().to_string())
}

fn detect(path: &Path, ffmpeg: &str) -> Result<Container> {
  let mut start = [0u8; 16];
  let read = std::fs::File::open(path)
    .and_then(|mut file| file.read(&mut start))
    .with_context(|| format!("failed to read {}", path.display()))
    .code("io_error")?;
  let start = &start[..read];
  // RPU files start with an RPU, with or without its NAL unit header, which raw HEVC never does.
  if start.starts_with(&[0, 0, 0, 1, 0x7c, 0x01, 0x19]) || start.starts_with(&[0, 0, 0, 1, 0x19]) {
    return Ok(Container::RpuFile);
  }
  if rabbit_av1::framing::detect(start).is_some() {
    return Ok(Container::Av1);
  }
  // Raw HEVC starts with a parameter set or access unit delimiter after a start code.
  let nal = match start {
    [0, 0, 0, 1, header, ..] | [0, 0, 1, header, ..] => Some((header >> 1) & 0x3f),
    _ => None,
  };
  if matches!(nal, Some(32..=35 | 39)) {
    return Ok(Container::Hevc);
  }
  match ffprobe_codec(path, ffmpeg)?.as_str() {
    "hevc" => Ok(Container::Hevc),
    "av1" => Ok(Container::Av1),
    "" => Err(fail("unsupported_input", format!("ffprobe found no video in {}", path.display()))),
    other => Err(fail(
      "unsupported_input",
      format!(
        "{} is {other} video, Dolby Vision is read from HEVC, AV1 or an RPU file",
        path.display()
      ),
    )),
  }
}

pub fn read(path: &Path, ffmpeg: &str, fps: Option<(u32, u32)>) -> Result<DolbyVision> {
  if !path.is_file() {
    return Err(fail("input_not_found", format!("input file not found: {}", path.display())));
  }
  let container = detect(path, ffmpeg)?;
  let rpus = match container {
    Container::RpuFile => dolby_vision::rpu::utils::parse_rpu_file(path).map_err(|error| {
      fail(
        "invalid_dolby_vision",
        format!("{} is not a valid RPU file: {error:#}", path.display()),
      )
    })?,
    Container::Hevc => hevc_rpus(path, ffmpeg)?,
    Container::Av1 => av1_rpus(&Source { path, ffmpeg, fps })?,
  };
  let Some(first) = rpus.first() else {
    return Err(fail("no_dolby_vision", format!("{} has no Dolby Vision metadata", path.display())));
  };
  Ok(DolbyVision {
    container,
    profile: first.dovi_profile,
    level6: rpus.iter().find_map(level6_of),
    frames: rpus.iter().map(frame_of).collect(),
  })
}

fn av1_rpus(source: &Source) -> Result<Vec<DoviRpu>> {
  let mut input = Input::open(source.path, source.ffmpeg, source.fps)?;
  let mut parser = StreamParser::new();
  let mut rpus = Vec::new();
  let mut missing = 0usize;
  let mut shown = 0usize;
  let result = (|| -> Result<()> {
    while let Some(packet) = input.reader.next_packet()? {
      crate::cancel::check()?;
      let obus = parser.parse_packet(&packet.data)?;
      let mut found = None;
      let mut frames = 0;
      for obu in &obus {
        match &obu.kind {
          ObuKind::Metadata(Metadata::ItuTT35(t35))
            if found.is_none() && crate::hdr10plus::classify(t35) == crate::hdr10plus::T35Kind::DolbyVision =>
          {
            // The OBU payload after the metadata type, as Dolby Vision tools read it.
            let payload = &packet.data[obu.payload.clone()];
            found = Some(
              DoviRpu::parse_itu_t35_dovi_metadata_obu(&payload[1..])
                .map_err(|error| fail("invalid_dolby_vision", format!("frame {shown}: {error:#}")))?,
            );
          }
          ObuKind::Frame(frame) if frame.header.show_frame || frame.header.show_existing_frame => frames += 1,
          _ => {}
        }
      }
      if frames == 0 {
        continue;
      }
      shown += frames;
      match found {
        Some(rpu) => rpus.push(rpu),
        None => missing += 1,
      }
    }
    Ok(())
  })();
  if let Err(error) = result {
    return Err(if crate::cancel::requested() {
      error
    } else {
      explain(&mut input, error)
    });
  }
  input.finish()?;
  if missing > 0 && !rpus.is_empty() {
    return Err(fail(
      "missing_dolby_vision",
      format!("{missing} of {shown} frames have no Dolby Vision metadata"),
    ));
  }
  Ok(rpus)
}

// Collects the RPU of every frame from Annex B HEVC in decoding order, then puts them in
// display order by the picture order count of their frames, as dovi_tool does.
fn collect_hevc(reader: &mut dyn Read) -> Result<Vec<DoviRpu>> {
  const CHUNK: usize = 1 << 20;
  let mut parser = HevcParser::default();
  let mut chunk: Vec<u8> = Vec::with_capacity(2 * CHUNK);
  let mut buffer = vec![0u8; CHUNK];
  let mut offsets = Vec::new();
  let mut found: Vec<(u64, DoviRpu)> = Vec::new();
  let mut ended = false;
  while !ended {
    crate::cancel::check()?;
    // Always read more than what is left over, so a NAL unit larger than a chunk completes.
    let goal = chunk.len() + CHUNK;
    while chunk.len() < goal {
      let read = reader
        .read(&mut buffer)
        .context("failed to read the HEVC stream")
        .code("io_error")?;
      if read == 0 {
        ended = true;
        break;
      }
      chunk.extend_from_slice(&buffer[..read]);
    }
    parser.get_offsets(&chunk, &mut offsets);
    let Some(&last) = offsets.last() else {
      if ended {
        break;
      }
      continue;
    };
    // Unless the stream ended, the last NAL unit may be incomplete and waits for the next chunk.
    let carry = if ended {
      None
    } else {
      offsets.pop();
      if offsets.is_empty() {
        continue;
      }
      Some(chunk[last..].to_vec())
    };
    let nals = parser
      .split_nals(&chunk, &offsets, last, true)
      .map_err(|error| fail("invalid_bitstream", format!("the HEVC stream is damaged: {error:#}")))?;
    for nal in &nals {
      // Like dovi_tool, a frame keeps its first RPU.
      if nal.nal_type == NAL_UNSPEC62 && found.last().is_none_or(|(index, _)| *index != nal.decoded_frame_index) {
        let rpu = DoviRpu::parse_unspec62_nalu(&chunk[nal.start..nal.end]).map_err(|error| {
          fail(
            "invalid_dolby_vision",
            format!("decoded frame {}: {error:#}", nal.decoded_frame_index),
          )
        })?;
        found.push((nal.decoded_frame_index, rpu));
      }
    }
    chunk.clear();
    if let Some(carry) = carry {
      chunk.extend(carry);
    }
  }
  parser.finish();
  let frames = parser.ordered_frames();
  if found.is_empty() {
    return Ok(Vec::new());
  }
  if found.len() != frames.len() {
    return Err(fail(
      "missing_dolby_vision",
      format!(
        "{} of {} frames have no Dolby Vision metadata",
        frames.len().saturating_sub(found.len()),
        frames.len()
      ),
    ));
  }
  let mut order = std::collections::HashMap::with_capacity(frames.len());
  for frame in frames {
    order.insert(frame.decoded_number, frame.presentation_number);
  }
  let mut placed = Vec::with_capacity(found.len());
  for (decoded, rpu) in found {
    let presentation = *order.get(&decoded).ok_or_else(|| {
      fail(
        "invalid_bitstream",
        format!("Dolby Vision metadata for decoded frame {decoded}, which the stream does not have"),
      )
    })?;
    placed.push((presentation, rpu));
  }
  placed.sort_by_key(|(presentation, _)| *presentation);
  Ok(placed.into_iter().map(|(_, rpu)| rpu).collect())
}

fn hevc_rpus(path: &Path, ffmpeg: &str) -> Result<Vec<DoviRpu>> {
  let raw = matches!(detect_raw(path), Some(true));
  if raw {
    let file = std::fs::File::open(path)
      .with_context(|| format!("failed to open {}", path.display()))
      .code("io_error")?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    return collect_hevc(&mut reader);
  }
  // ffmpeg copies the HEVC track out of its container as Annex B, without decoding it.
  let mut child = Command::new(ffmpeg)
    .args(["-v", "error", "-nostdin", "-i"])
    .arg(path)
    .args(["-map", "0:V:0", "-c:v", "copy", "-bsf:v", "hevc_mp4toannexb", "-f", "hevc", "-"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()
    .with_context(|| format!("failed to run {ffmpeg}"))
    .code("ffmpeg_missing")?;
  let mut stdout = child.stdout.take().unwrap();
  let result = collect_hevc(&mut stdout);
  drop(stdout);
  if result.is_err() {
    let _ = child.kill();
  }
  let status = child.wait().context("failed to wait for ffmpeg").code("ffmpeg_failed")?;
  let rpus = result?;
  if !status.success() {
    return Err(fail(
      "ffmpeg_failed",
      format!("ffmpeg failed while reading {} ({status})", path.display()),
    ));
  }
  Ok(rpus)
}

fn detect_raw(path: &Path) -> Option<bool> {
  let mut start = [0u8; 5];
  std::fs::File::open(path).ok()?.read_exact(&mut start).ok()?;
  Some(matches!(start, [0, 0, 0, 1, _] | [0, 0, 1, _, _]))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn frames(cuts: &[usize], level1: impl Fn(usize) -> Option<[u16; 3]>, count: usize) -> DolbyVision {
    DolbyVision {
      container: Container::RpuFile,
      profile: 8,
      level6: None,
      frames: (0..count)
        .map(|index| Frame {
          scene_cut: cuts.contains(&index),
          level1: level1(index),
        })
        .collect(),
    }
  }

  #[test]
  fn finds_scene_starts() {
    let shots = |index: usize| Some([0, if index < 4 { 2500 } else { 3000 }, 1500]);
    let flagged = frames(&[0, 4], shots, 10);
    let (starts, source) = flagged.scene_starts();
    assert_eq!(source, CutSource::ShotFlags);
    assert_eq!(starts.unwrap().iter().filter(|&&start| start).count(), 2);

    let every = frames(&(0..10).collect::<Vec<_>>(), shots, 10);
    let (starts, source) = every.scene_starts();
    assert_eq!(source, CutSource::Level1);
    assert!(starts.unwrap()[4]);

    let noisy = frames(&[], |index| Some([0, 2000 + index as u16, 1000]), 10);
    assert_eq!(noisy.scene_starts(), (None, CutSource::None));
  }
}
