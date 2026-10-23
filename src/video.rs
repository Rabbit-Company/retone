// Reading AV1 video for its HDR metadata, and rewriting it.

use crate::hdr10plus::{Hdr10Plus, Issue, Severity, T35Kind, classify, misplaced_type};
use crate::report::{Coded, fail};
use anyhow::Result;
use rabbit_av1::bits::BitWriter;
use rabbit_av1::metadata::{ContentLightLevel, MasteringDisplay, Metadata};
use rabbit_av1::obu::{OBU_SEQUENCE_HEADER, OBU_TILE_GROUP};
use rabbit_av1::sequence::{CP_BT_2020, MC_BT_2020_NCL, TC_SMPTE_2084};
use rabbit_av1::{ErrorKind, Input, Obu, ObuKind, Output, Packet, SequenceHeader, StreamParser, Written};
use std::path::Path;

pub struct Source<'a> {
  pub path: &'a Path,
  pub ffmpeg: &'a str,
  pub fps: Option<(u32, u32)>,
}

// One HDR10+ metadata OBU as found in the stream.
#[derive(Clone, Debug)]
pub enum Found {
  Parsed(Hdr10Plus),
  Damaged(String),
}

pub struct Frame {
  pub temporal_unit: usize,
  pub seconds: f64,
  pub hdr10plus: Vec<Found>,
}

#[derive(Clone, Debug)]
pub struct Finding {
  pub severity: Severity,
  pub code: &'static str,
  pub message: String,
  pub frame: Option<usize>,
  pub temporal_unit: Option<usize>,
}

impl Finding {
  pub fn from_issue(issue: Issue, frame: Option<usize>, temporal_unit: Option<usize>) -> Finding {
    Finding {
      severity: issue.severity,
      code: issue.code,
      message: issue.message,
      frame,
      temporal_unit,
    }
  }
}

#[derive(Default)]
pub struct Scan {
  pub sequence: Option<SequenceHeader>,
  pub sequence_headers: usize,
  pub sequence_changes: bool,
  pub temporal_units: usize,
  pub coded_frames: usize,
  pub frames: Vec<Frame>,
  pub content_light: Vec<ContentLightLevel>,
  pub mastering_display: Vec<MasteringDisplay>,
  pub static_units: usize,
  pub dolby_vision: usize,
  pub other_t35: usize,
  // Several spatial layers, so a temporal unit shows a frame per layer. Temporal layers alone
  // keep one shown frame per temporal unit and need nothing special.
  pub layered: bool,
  pub findings: Vec<Finding>,
}

impl Scan {
  pub fn hdr10plus_frames(&self) -> usize {
    self.frames.iter().filter(|frame| !frame.hdr10plus.is_empty()).count()
  }

  // Frames per second from the display times, since a container's time base is no frame rate.
  pub fn frame_rate(&self) -> Option<f64> {
    let (first, last) = (self.frames.first()?, self.frames.last()?);
    let span = last.seconds - first.seconds;
    (self.frames.len() > 1 && span > 0.0).then(|| (self.frames.len() - 1) as f64 / span)
  }

  fn push(&mut self, severity: Severity, code: &'static str, message: impl Into<String>, frame: Option<usize>) {
    let temporal_unit = Some(self.temporal_units.saturating_sub(1));
    self.findings.push(Finding {
      severity,
      code,
      message: message.into(),
      frame,
      temporal_unit,
    });
  }

  // The parsed metadata of every shown frame, in display order.
  pub fn metadata(&self) -> Result<Vec<Hdr10Plus>> {
    let mut list = Vec::with_capacity(self.frames.len());
    for (index, frame) in self.frames.iter().enumerate() {
      match frame.hdr10plus.first() {
        Some(Found::Parsed(meta)) => list.push(meta.clone()),
        Some(Found::Damaged(message)) => return Err(fail("invalid_hdr10plus", format!("frame {index}: {message}"))),
        None => {
          return Err(fail(
            "missing_metadata",
            format!(
              "frame {index} has no HDR10+ metadata, {} of {} frames have it",
              self.hdr10plus_frames(),
              self.frames.len()
            ),
          ));
        }
      }
    }
    Ok(list)
  }
}

// When reading fails while ffmpeg feeds the input, ffmpeg usually stopped first.
pub fn explain(input: &mut Input, error: anyhow::Error) -> anyhow::Error {
  let status = input.explain(rabbit_av1::Error::new(ErrorKind::Io, "reading stopped"));
  if status.kind() == ErrorKind::FfmpegFailed {
    status.into()
  } else {
    error
  }
}

pub fn scan(source: &Source) -> Result<Scan> {
  let mut input = Input::open(source.path, source.ffmpeg, source.fps)?;
  let mut result = Scan { ..Scan::default() };
  let mut parser = StreamParser::new();
  let outcome = (|| -> Result<()> {
    while let Some(packet) = input.reader.next_packet()? {
      crate::cancel::check()?;
      result.temporal_units += 1;
      let seconds = input.reader.timestamp(packet.pts, 1_000_000) as f64 / 1e6;
      let obus = parser
        .parse_packet(&packet.data)
        .map_err(|error| error.context(format!("temporal unit {}", result.temporal_units)))?;
      scan_unit(&mut result, &obus, seconds);
    }
    Ok(())
  })();
  if let Err(error) = outcome {
    return Err(if crate::cancel::requested() {
      error
    } else {
      explain(&mut input, error)
    });
  }
  input.finish()?;
  if result.temporal_units == 0 {
    return Err(fail(
      "unsupported_input",
      format!("no AV1 frames found in {}", source.path.display()),
    ));
  }
  finish_scan(&mut result);
  Ok(result)
}

fn scan_unit(result: &mut Scan, obus: &[Obu], seconds: f64) {
  use Severity::Error;
  // HDR10+ OBUs since the last OBU of the previous frame. They belong to the next frame.
  let mut pending: Vec<Found> = Vec::new();
  let mut has_static = false;
  for obu in obus {
    if obu.extension.is_some_and(|extension| extension.spatial_id > 0) {
      result.layered = true;
    }
    match &obu.kind {
      ObuKind::Sequence(sequence) => {
        if !pending.is_empty() {
          result.push(
            Error,
            "placement",
            "HDR10+ metadata comes before the sequence header instead of after it",
            None,
          );
          pending.clear();
        }
        result.sequence_headers += 1;
        match &result.sequence {
          None => result.sequence = Some(sequence.clone()),
          Some(first)
            if first.color_primaries != sequence.color_primaries || first.transfer_characteristics != sequence.transfer_characteristics =>
          {
            result.sequence_changes = true
          }
          Some(_) => {}
        }
        if sequence.operating_points.iter().any(|point| (point.idc >> 8).count_ones() > 1) {
          result.layered = true;
        }
      }
      ObuKind::Metadata(metadata) => match metadata {
        Metadata::ContentLightLevel(cll) => {
          has_static = true;
          if !result.content_light.contains(cll) {
            result.content_light.push(*cll);
          }
        }
        Metadata::MasteringDisplay(mdcv) => {
          has_static = true;
          if !result.mastering_display.contains(mdcv) {
            result.mastering_display.push(*mdcv);
          }
        }
        Metadata::ItuTT35(t35) => match classify(t35) {
          T35Kind::Hdr10Plus => pending.push(match Hdr10Plus::parse(&t35.payload) {
            Ok(meta) => Found::Parsed(meta),
            Err(error) => Found::Damaged(error.to_string()),
          }),
          T35Kind::DolbyVision => result.dolby_vision += 1,
          T35Kind::BrokenHdr10Plus(field) => {
            let message = match field {
              "country_code" => format!("HDR10+ metadata has country code {:#04x} instead of 0xb5", t35.country_code),
              _ => format!("HDR10+ metadata has a wrong {}", field.replace('_', " ")),
            };
            result.push(Error, field, message, None);
          }
          T35Kind::Other => result.other_t35 += 1,
        },
        other if misplaced_type(other) => result.push(
          Error,
          "metadata_type",
          format!(
            "HDR10+ metadata is in a metadata OBU of type {} instead of ITU-T T.35 (4)",
            other.metadata_type()
          ),
          None,
        ),
        _ => {}
      },
      ObuKind::Frame(frame) => {
        result.coded_frames += usize::from(!frame.header.show_existing_frame);
        let shown = frame.header.show_frame || frame.header.show_existing_frame;
        if shown {
          result.frames.push(Frame {
            temporal_unit: result.temporal_units - 1,
            seconds,
            hdr10plus: std::mem::take(&mut pending),
          });
        } else if !pending.is_empty() {
          result.push(
            Error,
            "placement",
            "HDR10+ metadata comes before a hidden frame instead of right before the frame that is shown",
            None,
          );
          pending.clear();
        }
      }
      // Repeated frame headers and tile groups still belong to the frame before.
      ObuKind::FrameCopy | ObuKind::Other
        if !pending.is_empty() && (obu.obu_type == OBU_TILE_GROUP || matches!(obu.kind, ObuKind::FrameCopy)) =>
      {
        result.push(Error, "placement", "HDR10+ metadata comes inside a frame", None);
        pending.clear();
      }
      _ => {}
    }
  }
  if !pending.is_empty() {
    result.push(
      Error,
      "placement",
      "HDR10+ metadata comes after the last frame of the temporal unit",
      None,
    );
  }
  if has_static {
    result.static_units += 1;
  }
}

fn finish_scan(result: &mut Scan) {
  use Severity::{Error, Warning};
  if result.layered {
    // The specification leaves streams with layers out, so frames are not counted against it.
    result.push(
      Warning,
      "layered_stream",
      "the stream has several spatial layers, which the HDR10+ AV1 specification does not cover",
      None,
    );
    return;
  }
  let with_metadata = result.hdr10plus_frames();
  let placement_errors = result.findings.iter().any(|finding| finding.code == "placement");
  if with_metadata == 0 && !placement_errors {
    return;
  }
  let mut findings = Vec::new();
  for (index, frame) in result.frames.iter().enumerate() {
    let temporal_unit = Some(frame.temporal_unit);
    match frame.hdr10plus.len() {
      0 => findings.push(Finding {
        severity: Error,
        code: "missing",
        message: "a shown frame has no HDR10+ metadata".to_string(),
        frame: Some(index),
        temporal_unit,
      }),
      1 => {}
      count => findings.push(Finding {
        severity: Error,
        code: "duplicate",
        message: format!("a shown frame has {count} HDR10+ metadata OBUs instead of 1"),
        frame: Some(index),
        temporal_unit,
      }),
    }
    for found in &frame.hdr10plus {
      match found {
        Found::Parsed(meta) => {
          for issue in meta.validate() {
            findings.push(Finding::from_issue(issue, Some(index), temporal_unit));
          }
        }
        Found::Damaged(message) => findings.push(Finding {
          severity: Error,
          code: "damaged",
          message: message.clone(),
          frame: Some(index),
          temporal_unit,
        }),
      }
    }
  }
  result.findings.extend(findings);
}

// Rewrites every temporal unit with `edit` and writes the result under a temporary name.
pub fn rewrite(source: &Source, output: &Path, mut edit: impl FnMut(&mut Packet, &[Obu], &StreamParser) -> Result<()>) -> Result<Written> {
  let mut input = Input::open(source.path, source.ffmpeg, source.fps)?;
  let mut writer = Output::create(output, source.path, &input.reader, source.ffmpeg)?;
  let mut parser = StreamParser::new();
  let mut units = 0usize;
  let result = (|| -> Result<()> {
    while let Some(mut packet) = input.reader.next_packet()? {
      crate::cancel::check()?;
      units += 1;
      let obus = parser
        .parse_packet(&packet.data)
        .map_err(|error| error.context(format!("temporal unit {units}")))?;
      edit(&mut packet, &obus, &parser)?;
      writer.write(&packet)?;
    }
    Ok(())
  })();
  if let Err(error) = result {
    writer.discard();
    return Err(if crate::cancel::requested() {
      error
    } else {
      explain(&mut input, error)
    });
  }
  if let Err(error) = input.finish().map_err(anyhow::Error::from).and_then(|()| crate::cancel::check()) {
    writer.discard();
    return Err(error);
  }
  let written = writer.finish()?;
  if let Err(error) = crate::cancel::check() {
    written.discard();
    return Err(error);
  }
  Ok(written)
}

// Inserts OBUs before the OBUs at the given indexes, removing the OBUs `remove` marks, and keeps
// Annex B frame units intact. Indexes refer to the packet before any change.
pub fn edit_packet(packet: &mut Packet, remove: &[bool], insert: &[(usize, Vec<u8>)]) -> Result<()> {
  let mut index = 0;
  packet
    .retain_obus(|_, _| {
      let keep = !remove.get(index).copied().unwrap_or(false);
      index += 1;
      keep
    })
    .code("invalid_bitstream")?;
  // Work from the back, so earlier positions stay valid.
  let mut inserts: Vec<&(usize, Vec<u8>)> = insert.iter().collect();
  inserts.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
  for (at, obu) in inserts {
    let removed_before = remove.iter().take(*at).filter(|&&removed| removed).count();
    packet.insert_obu(at - removed_before, obu).code("invalid_bitstream")?;
  }
  Ok(())
}

// Sets the colour description of a sequence header to BT.2020 primaries, PQ and the BT.2020
// non-constant luminance matrix, as HDR10 and HDR10+ need.
pub fn set_hdr10_color(payload: &[u8], sequence: &SequenceHeader) -> Result<Vec<u8>> {
  if sequence.color_description_present
    && sequence.color_primaries == 1
    && sequence.transfer_characteristics == 13
    && sequence.matrix_coefficients == 0
  {
    return Err(fail("unsupported_input", "the video is coded as sRGB, which cannot carry HDR10"));
  }
  let Some(last) = payload.iter().rposition(|&byte| byte != 0) else {
    return Err(fail("invalid_bitstream", "the sequence header has no trailing bits"));
  };
  let end = last * 8 + 7 - payload[last].trailing_zeros() as usize;
  let after = sequence.color_description_bit + 1 + if sequence.color_description_present { 24 } else { 0 };
  if after > end {
    return Err(fail("invalid_bitstream", "the sequence header ends inside its colour description"));
  }
  let mut w = BitWriter::new();
  w.copy_bits(payload, 0, sequence.color_description_bit);
  w.bit(true);
  w.bits(CP_BT_2020 as u32, 8);
  w.bits(TC_SMPTE_2084 as u32, 8);
  w.bits(MC_BT_2020_NCL as u32, 8);
  w.copy_bits(payload, after, end);
  w.trailing_bits();
  let patched = w.finish();
  // Everything but the colour description must read back the same.
  let check = SequenceHeader::parse(&patched).code("invalid_bitstream")?;
  let same = SequenceHeader {
    color_description_present: true,
    color_primaries: CP_BT_2020,
    transfer_characteristics: TC_SMPTE_2084,
    matrix_coefficients: MC_BT_2020_NCL,
    color_description_bit: check.color_description_bit,
    film_grain_bit: check.film_grain_bit,
    ..sequence.clone()
  };
  if check != same {
    return Err(fail(
      "invalid_bitstream",
      "changing the colour description would change the sequence header",
    ));
  }
  Ok(patched)
}

// Rebuilds an OBU around a new payload, keeping its header.
pub fn replace_payload(packet: &Packet, obu: &Obu, payload: &[u8]) -> Vec<u8> {
  let mut out = packet.data[obu.prefix.clone()].to_vec();
  out[0] |= 0x02;
  rabbit_av1::bits::write_leb128(&mut out, payload.len() as u64);
  out.extend_from_slice(payload);
  out
}

pub fn is_sequence(obu: &Obu) -> bool {
  obu.obu_type == OBU_SEQUENCE_HEADER
}

#[cfg(test)]
mod tests {
  use super::*;
  use rabbit_av1::bits::BitReader;

  // A Main profile 10 bit header with timing info, without a colour description.
  fn header(description: Option<(u8, u8, u8)>) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(0, 3);
    w.bit(false);
    w.bit(false);
    w.bit(false); // no timing info
    w.bit(false); // initial_display_delay_present_flag
    w.bits(0, 5);
    w.bits(0, 12);
    w.bits(8, 5);
    w.bit(false);
    w.bits(10, 4);
    w.bits(10, 4);
    w.bits(1919, 11);
    w.bits(1079, 11);
    w.bit(false);
    w.bit(false);
    w.bits(0b11, 2);
    w.bits(0b11, 2);
    w.bit(true);
    w.bit(true);
    w.bit(true);
    w.bit(true);
    w.bit(true);
    w.bit(true);
    w.bit(true);
    w.bits(6, 3);
    w.bit(false);
    w.bit(true);
    w.bit(true);
    w.bit(true); // high_bitdepth
    w.bit(false); // mono_chrome
    match description {
      Some((primaries, transfer, matrix)) => {
        w.bit(true);
        w.bits(primaries as u32, 8);
        w.bits(transfer as u32, 8);
        w.bits(matrix as u32, 8);
      }
      None => w.bit(false),
    }
    w.bit(true); // color_range
    w.bits(1, 2);
    w.bit(false);
    w.bit(true); // film_grain_params_present
    w.trailing_bits();
    w.finish()
  }

  #[test]
  fn sets_the_colour_description() {
    for description in [None, Some((1, 1, 1)), Some((9, 16, 9))] {
      let original = header(description);
      let sequence = SequenceHeader::parse(&original).unwrap();
      let patched = set_hdr10_color(&original, &sequence).unwrap();
      let read = SequenceHeader::parse(&patched).unwrap();
      assert!(read.is_pq() && read.color_primaries == 9 && read.matrix_coefficients == 9);
      assert!(read.full_range && read.film_grain_params_present);
      assert_eq!(read.chroma_sample_position, 1);
      assert_eq!(patched, header(Some((9, 16, 9))));
      let mut r = BitReader::new(&patched);
      r.skip(read.film_grain_bit + 1).unwrap();
      assert!(r.bit().unwrap(), "trailing one bit");
    }
    let srgb = header(Some((1, 13, 0)));
    assert!(set_hdr10_color(&srgb, &SequenceHeader::parse(&srgb).unwrap()).is_err());
  }
}
