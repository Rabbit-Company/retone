// Describing and checking HDR metadata.

use crate::edit::content_light_from;
use crate::hdr10plus::{Hdr10Plus, Severity};
use crate::video::{Finding, Found, Scan};
use rabbit_av1::SequenceHeader;

pub fn primaries_name(value: u8) -> &'static str {
  match value {
    1 => "BT.709",
    2 => "unspecified",
    9 => "BT.2020",
    11 => "DCI-P3",
    12 => "Display P3",
    _ => "other",
  }
}

pub fn transfer_name(value: u8) -> &'static str {
  match value {
    1 | 6 | 14 | 15 => "SDR",
    2 => "unspecified",
    13 => "sRGB",
    16 => "PQ",
    18 => "HLG",
    _ => "other",
  }
}

pub fn matrix_name(value: u8) -> &'static str {
  match value {
    0 => "identity",
    1 => "BT.709",
    2 => "unspecified",
    9 => "BT.2020 NCL",
    10 => "BT.2020 CL",
    14 => "ICtCp",
    _ => "other",
  }
}

pub fn clock(seconds: f64) -> String {
  let millis = (seconds.max(0.0) * 1000.0).round() as u64;
  format!(
    "{}:{:02}:{:02}.{:03}",
    millis / 3_600_000,
    millis / 60_000 % 60,
    millis / 1000 % 60,
    millis % 1000
  )
}

// Runs of frames with the same metadata, as start and length.
pub fn scenes(metadata: &[Hdr10Plus]) -> Vec<(usize, usize)> {
  let mut scenes: Vec<(usize, usize)> = Vec::new();
  for (index, meta) in metadata.iter().enumerate() {
    match scenes.last_mut() {
      Some((start, length)) if metadata[*start] == *meta => *length += 1,
      _ => scenes.push((index, 1)),
    }
  }
  scenes
}

// The HDR formats a stream carries, by name.
pub fn formats(scan: &Scan) -> Vec<&'static str> {
  let mut list = Vec::new();
  if let Some(sequence) = &scan.sequence {
    if sequence.is_pq() {
      list.push(if scan.mastering_display.is_empty() && scan.content_light.is_empty() {
        "PQ"
      } else {
        "HDR10"
      });
    }
    if sequence.is_hlg() {
      list.push("HLG");
    }
  }
  if scan.hdr10plus_frames() > 0 {
    list.push("HDR10+");
  }
  if scan.dolby_vision > 0 {
    list.push("Dolby Vision");
  }
  if list.is_empty() {
    list.push("SDR");
  }
  list
}

fn finding(severity: Severity, code: &'static str, message: impl Into<String>) -> Finding {
  Finding {
    severity,
    code,
    message: message.into(),
    frame: None,
    temporal_unit: None,
  }
}

// What the HDR10+ AV1 specification asks of the colour description, and how well the static
// metadata fits the dynamic metadata.
pub fn stream_findings(scan: &Scan) -> Vec<Finding> {
  use Severity::Warning;
  let mut findings = Vec::new();
  let has_hdr10plus = scan.hdr10plus_frames() > 0;
  if let Some(sequence) = &scan.sequence
    && has_hdr10plus
  {
    findings.extend(color_findings(sequence));
  }
  if scan.sequence_changes {
    findings.push(finding(
      Warning,
      "sequence_changes",
      "the colour description changes within the video",
    ));
  }
  if has_hdr10plus && scan.mastering_display.is_empty() {
    findings.push(finding(
      Warning,
      "no_mastering_display",
      "there is no mastering display metadata, which HDR10 and HDR10+ expect. Add it with retone static, unless the container carries it",
    ));
  }
  if scan.content_light.len() > 1 {
    findings.push(finding(
      Warning,
      "content_light_changes",
      "the content light level changes within the video",
    ));
  }
  if scan.mastering_display.len() > 1 {
    findings.push(finding(
      Warning,
      "mastering_display_changes",
      "the mastering display metadata changes within the video",
    ));
  }
  let metadata: Vec<Hdr10Plus> = scan
    .frames
    .iter()
    .filter_map(|frame| match frame.hdr10plus.first() {
      Some(Found::Parsed(meta)) => Some(meta.clone()),
      _ => None,
    })
    .collect();
  if !metadata.is_empty() {
    let measured = content_light_from(&metadata);
    if let Some(cll) = scan.content_light.first()
      && measured.max_cll > cll.max_cll.saturating_add(cll.max_cll / 20)
    {
      findings.push(finding(
        Warning,
        "max_cll_low",
        format!(
          "MaxCLL is {} cd/m², but the HDR10+ metadata reaches {} cd/m²",
          cll.max_cll, measured.max_cll
        ),
      ));
    }
    if let Some(mdcv) = scan.mastering_display.first()
      && measured.max_cll as f64 > mdcv.max_nits() * 1.05
    {
      findings.push(finding(
        Warning,
        "brighter_than_mastering_display",
        format!(
          "the HDR10+ metadata reaches {} cd/m², above the {} cd/m² of the mastering display",
          measured.max_cll,
          mdcv.max_nits()
        ),
      ));
    }
  }
  findings
}

pub fn color_findings(sequence: &SequenceHeader) -> Vec<Finding> {
  use Severity::{Error, Warning};
  let mut findings = Vec::new();
  if sequence.color_primaries != 9 {
    findings.push(finding(
      Error,
      "color_primaries",
      format!(
        "color_primaries is {} ({}), HDR10+ needs 9 (BT.2020)",
        sequence.color_primaries,
        primaries_name(sequence.color_primaries)
      ),
    ));
  }
  if sequence.transfer_characteristics != 16 {
    findings.push(finding(
      Error,
      "transfer_characteristics",
      format!(
        "transfer_characteristics is {} ({}), HDR10+ needs 16 (PQ)",
        sequence.transfer_characteristics,
        transfer_name(sequence.transfer_characteristics)
      ),
    ));
  }
  if sequence.matrix_coefficients != 9 {
    findings.push(finding(
      Error,
      "matrix_coefficients",
      format!(
        "matrix_coefficients is {} ({}), HDR10+ needs 9 (BT.2020 NCL)",
        sequence.matrix_coefficients,
        matrix_name(sequence.matrix_coefficients)
      ),
    ));
  }
  if sequence.full_range {
    findings.push(finding(
      Warning,
      "color_range",
      "the video is full range, the HDR10+ AV1 specification recommends limited range",
    ));
  }
  if sequence.mono_chrome {
    findings.push(finding(
      Warning,
      "mono_chrome",
      "the video is monochrome, the HDR10+ AV1 specification recommends colour",
    ));
  }
  if sequence.subsampling_x && sequence.subsampling_y && sequence.chroma_sample_position != 2 {
    findings.push(finding(
      Warning,
      "chroma_sample_position",
      format!(
        "chroma_sample_position is {}, the HDR10+ AV1 specification recommends 2 (top left)",
        sequence.chroma_sample_position
      ),
    ));
  }
  findings
}

pub fn check_metadata(metadata: &[Hdr10Plus]) -> Vec<Finding> {
  let mut findings = Vec::new();
  for (index, meta) in metadata.iter().enumerate() {
    for issue in meta.validate() {
      findings.push(Finding::from_issue(issue, Some(index), None));
    }
  }
  findings
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::hdr10plus::tests::sample;

  #[test]
  fn formats_clock_and_scenes() {
    assert_eq!(clock(3725.5), "1:02:05.500");
    let mut other = sample();
    other.windows[0].average_maxrgb = 1;
    assert_eq!(scenes(&[sample(), sample(), other, sample()]), vec![(0, 2), (2, 1), (3, 1)]);
  }
}
