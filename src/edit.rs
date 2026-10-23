// Writing and removing HDR metadata in AV1 video.

use crate::hdr10plus::{Hdr10Plus, Severity, T35Kind, classify, misplaced_type};
use crate::report::{Coded, fail};
use crate::video::{Found, Source, edit_packet, is_sequence, replace_payload, rewrite, scan, set_hdr10_color};
use anyhow::{Context, Result};
use rabbit_av1::metadata::{ContentLightLevel, MasteringDisplay, Metadata};
use rabbit_av1::{Obu, ObuKind, Written};
use std::path::Path;

fn is_hdr10plus(obu: &Obu) -> bool {
  match &obu.kind {
    ObuKind::Metadata(Metadata::ItuTT35(t35)) => matches!(classify(t35), T35Kind::Hdr10Plus | T35Kind::BrokenHdr10Plus(_)),
    ObuKind::Metadata(other) => misplaced_type(other),
    _ => false,
  }
}

fn is_dolby_vision(obu: &Obu) -> bool {
  matches!(&obu.kind, ObuKind::Metadata(Metadata::ItuTT35(t35)) if classify(t35) == T35Kind::DolbyVision)
}

fn is_static(obu: &Obu) -> bool {
  matches!(
    &obu.kind,
    ObuKind::Metadata(Metadata::ContentLightLevel(_) | Metadata::MasteringDisplay(_))
  )
}

fn starts_shown_frame(obu: &Obu) -> bool {
  matches!(&obu.kind, ObuKind::Frame(frame) if frame.header.show_frame || frame.header.show_existing_frame)
}

fn is_hdr10_color(sequence: &rabbit_av1::SequenceHeader) -> bool {
  sequence.color_primaries == 9 && sequence.transfer_characteristics == 16 && sequence.matrix_coefficients == 9
}

pub struct ApplyOptions {
  pub fix_color: bool,
  pub force_fix_color: bool,
  pub verify: bool,
}

// --fix-color corrects tags that are missing or wrong on PQ video. Video tagged with another
// transfer, such as BT.709 SDR, is a different picture and would look washed out as PQ, so
// retagging it needs --force-fix-color. HLG is never retagged.
fn refuse_retag(sequence: &rabbit_av1::SequenceHeader, force: bool) -> Option<anyhow::Error> {
  let transfer = sequence.transfer_characteristics;
  if sequence.is_hlg() || (!force && transfer != 2 && transfer != 16) {
    return Some(not_hdr10(sequence));
  }
  None
}

pub struct ApplyReport {
  pub frames: usize,
  pub replaced: usize,
  pub color_fixed: bool,
  pub warnings: usize,
  pub has_mastering_display: bool,
  pub verified: bool,
}

fn not_hdr10(sequence: &rabbit_av1::SequenceHeader) -> anyhow::Error {
  if sequence.is_hlg() {
    return fail(
      "not_hdr10",
      "the video is HLG, and HDR10+ needs PQ. --fix-color only corrects the tags of video that is PQ already",
    );
  }
  if sequence.transfer_characteristics != 2 && sequence.transfer_characteristics != 16 {
    return fail(
      "not_hdr10",
      format!(
        "the video is tagged with transfer {} ({}), not PQ. Tagging SDR video as PQ makes it look washed out, so --fix-color only retags untagged video. Pass --force-fix-color as well when the picture is PQ and only its tags are wrong",
        sequence.transfer_characteristics,
        crate::inspect::transfer_name(sequence.transfer_characteristics)
      ),
    );
  }
  fail(
    "not_hdr10",
    format!(
      "the video is tagged with primaries {}, transfer {} and matrix {}, HDR10+ needs 9, 16 and 9 (BT.2020, PQ). Pass --fix-color when the picture is BT.2020 PQ and only the tags are wrong",
      sequence.color_primaries, sequence.transfer_characteristics, sequence.matrix_coefficients
    ),
  )
}

// Reads the written stream back and lets dav1d decode it.
fn verify(written: &Written, source: &Source, expected: &[Hdr10Plus]) -> Result<()> {
  let check = Source {
    path: &written.temp,
    ffmpeg: source.ffmpeg,
    fps: source.fps,
  };
  let result = scan(&check).code("verify_failed")?;
  if let Some(finding) = result.findings.iter().find(|finding| finding.severity == Severity::Error) {
    return Err(fail("verify_failed", format!("the output breaks a rule: {}", finding.message)));
  }
  if result.frames.len() != expected.len() {
    return Err(fail(
      "verify_failed",
      format!("the output shows {} frames instead of {}", result.frames.len(), expected.len()),
    ));
  }
  for (index, (frame, meta)) in result.frames.iter().zip(expected).enumerate() {
    if !matches!(frame.hdr10plus.as_slice(), [Found::Parsed(found)] if found == meta) {
      return Err(fail("verify_failed", format!("frame {index} does not carry its HDR10+ metadata")));
    }
  }
  let (decoded, messages) = rabbit_av1::container::decode_messages(&written.temp, source.ffmpeg).map_err(|error| {
    if error.kind() == rabbit_av1::ErrorKind::DecoderMissing {
      fail(
        "verify_unavailable",
        format!("{} has no libdav1d decoder, which --verify needs", source.ffmpeg),
      )
    } else {
      error.into()
    }
  })?;
  if !decoded || !messages.is_empty() {
    let (_, known) = rabbit_av1::container::decode_messages(source.path, source.ffmpeg)?;
    if let Some(message) = messages.iter().find(|message| !known.contains(message)) {
      return Err(fail("verify_failed", format!("dav1d could not decode the output: {message}")));
    }
  }
  Ok(())
}

pub fn apply(source: &Source, output: &Path, metadata: &[Hdr10Plus], options: &ApplyOptions) -> Result<ApplyReport> {
  let before = scan(source)?;
  if before.layered {
    return Err(fail(
      "unsupported_input",
      "the video has several spatial layers, which the HDR10+ AV1 specification does not cover",
    ));
  }
  let sequence = before
    .sequence
    .clone()
    .context("the video has no sequence header")
    .code("invalid_bitstream")?;
  let color_fixed = !is_hdr10_color(&sequence);
  if color_fixed && !options.fix_color {
    return Err(not_hdr10(&sequence));
  }
  if color_fixed && let Some(error) = refuse_retag(&sequence, options.force_fix_color) {
    return Err(error);
  }
  if metadata.len() != before.frames.len() {
    return Err(fail(
      "frame_count_mismatch",
      format!(
        "the metadata has {} frames and the video shows {}",
        metadata.len(),
        before.frames.len()
      ),
    ));
  }
  let mut warnings = 0;
  for (index, meta) in metadata.iter().enumerate() {
    for issue in meta.validate() {
      if issue.severity == Severity::Error {
        return Err(fail("invalid_metadata", format!("frame {index}: {}", issue.message)));
      }
      warnings += 1;
    }
  }
  let encoded: Vec<Vec<u8>> = metadata.iter().map(|meta| meta.to_metadata().to_obu(None)).collect();
  let mut next = 0usize;
  let written = rewrite(source, output, |packet, obus, _| {
    let mut remove = vec![false; obus.len()];
    let mut insert = Vec::new();
    for (index, obu) in obus.iter().enumerate() {
      if is_hdr10plus(obu) {
        remove[index] = true;
      }
      if options.fix_color
        && let ObuKind::Sequence(sequence) = &obu.kind
        && !is_hdr10_color(sequence)
      {
        let payload = set_hdr10_color(&packet.data[obu.payload.clone()], sequence)?;
        remove[index] = true;
        insert.push((index, replace_payload(packet, obu, &payload)));
      }
      if starts_shown_frame(obu) {
        let obu = encoded
          .get(next)
          .context("the video shows more frames than when it was counted")
          .code("invalid_bitstream")?;
        insert.push((index, obu.clone()));
        next += 1;
      }
    }
    edit_packet(packet, &remove, &insert)
  })?;
  let verified = if options.verify {
    if let Err(error) = verify(&written, source, metadata) {
      written.discard();
      return Err(error);
    }
    true
  } else {
    false
  };
  written.commit()?;
  Ok(ApplyReport {
    frames: metadata.len(),
    replaced: before.hdr10plus_frames(),
    color_fixed,
    warnings,
    has_mastering_display: !before.mastering_display.is_empty(),
    verified,
  })
}

#[derive(Clone, Copy)]
pub struct RemoveOptions {
  pub hdr10plus: bool,
  pub dolby_vision: bool,
  pub static_metadata: bool,
}

#[derive(Default)]
pub struct RemoveReport {
  pub hdr10plus: usize,
  pub dolby_vision: usize,
  pub static_metadata: usize,
}

pub fn remove(source: &Source, output: &Path, options: RemoveOptions) -> Result<RemoveReport> {
  let mut report = RemoveReport::default();
  let written = rewrite(source, output, |packet, obus, _| {
    let remove: Vec<bool> = obus
      .iter()
      .map(|obu| {
        let (hdr10plus, dolby, fixed) = (is_hdr10plus(obu), is_dolby_vision(obu), is_static(obu));
        let drop = (options.hdr10plus && hdr10plus) || (options.dolby_vision && dolby) || (options.static_metadata && fixed);
        if drop {
          report.hdr10plus += usize::from(hdr10plus);
          report.dolby_vision += usize::from(dolby);
          report.static_metadata += usize::from(fixed);
        }
        drop
      })
      .collect();
    edit_packet(packet, &remove, &[])
  })?;
  written.commit()?;
  Ok(report)
}

pub struct StaticOptions {
  pub force_fix_color: bool,
  pub content_light: Option<ContentLightLevel>,
  pub mastering_display: Option<MasteringDisplay>,
  pub fix_color: bool,
}

pub struct StaticReport {
  pub sequence_headers: usize,
  pub color_fixed: bool,
}

pub fn write_static(source: &Source, output: &Path, options: &StaticOptions) -> Result<StaticReport> {
  let mut sequence_headers = 0;
  let mut color_fixed = false;
  let written = rewrite(source, output, |packet, obus, _| {
    let mut remove = vec![false; obus.len()];
    let mut insert = Vec::new();
    for (index, obu) in obus.iter().enumerate() {
      match &obu.kind {
        ObuKind::Metadata(Metadata::ContentLightLevel(_)) if options.content_light.is_some() => remove[index] = true,
        ObuKind::Metadata(Metadata::MasteringDisplay(_)) if options.mastering_display.is_some() => remove[index] = true,
        _ => {}
      }
      if let ObuKind::Sequence(sequence) = &obu.kind {
        sequence_headers += 1;
        if options.fix_color
          && !is_hdr10_color(sequence)
          && let Some(error) = refuse_retag(sequence, options.force_fix_color)
        {
          return Err(error);
        }
        if options.fix_color && !is_hdr10_color(sequence) {
          let payload = set_hdr10_color(&packet.data[obu.payload.clone()], sequence)?;
          remove[index] = true;
          insert.push((index, replace_payload(packet, obu, &payload)));
          color_fixed = true;
        }
      }
    }
    // The static metadata goes right after the last sequence header of the temporal unit.
    if let Some(last) = obus.iter().rposition(is_sequence) {
      if let Some(mdcv) = options.mastering_display {
        insert.push((last + 1, Metadata::MasteringDisplay(mdcv).to_obu(None)));
      }
      if let Some(cll) = options.content_light {
        insert.push((last + 1, Metadata::ContentLightLevel(cll).to_obu(None)));
      }
    }
    edit_packet(packet, &remove, &insert)
  })?;
  written.commit()?;
  Ok(StaticReport {
    sequence_headers,
    color_fixed,
  })
}

// MaxCLL and MaxFALL as HDR10+ metadata gives them: the brightest component of any frame, and
// the highest frame average of the brightest component per pixel.
pub fn content_light_from(metadata: &[Hdr10Plus]) -> ContentLightLevel {
  let mut cll = ContentLightLevel::default();
  for meta in metadata {
    if let Some(window) = meta.main() {
      let peak = window.maxscl.iter().copied().max().unwrap_or(0);
      cll.max_cll = cll.max_cll.max(((peak + 5) / 10).min(u16::MAX as u32) as u16);
      cll.max_fall = cll.max_fall.max(((window.average_maxrgb + 5) / 10).min(u16::MAX as u32) as u16);
    }
  }
  cll
}
