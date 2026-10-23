// HDR10+ metadata: SMPTE ST 2094-40 application 4, carried in ITU-T T.35 as CTA-861 Annex S
// defines it. Brightness values are in units of 0.1 cd/m², so 100000 is 10000 cd/m².

use crate::report::fail;
use anyhow::Result;
use rabbit_av1::bits::{BitReader, BitWriter};
use rabbit_av1::metadata::{ItuTT35, Metadata, T35_COUNTRY_US, T35_PROVIDER_DOLBY, T35_PROVIDER_SAMSUNG};

pub const PROVIDER_ORIENTED_CODE: u16 = 0x0001;
pub const APPLICATION_IDENTIFIER: u8 = 4;
pub const APPLICATION_VERSION: u8 = 1;
pub const PERCENTAGES_9: [u8; 9] = [1, 5, 10, 25, 50, 75, 90, 95, 99];
pub const PERCENTAGES_10: [u8; 10] = [1, 5, 10, 25, 50, 75, 90, 95, 98, 99];
pub const MAX_VALUE: u32 = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessingWindow {
  pub upper_left: [u16; 2],
  pub lower_right: [u16; 2],
  pub center_of_ellipse: [u16; 2],
  pub rotation_angle: u8,
  pub semimajor_axis_internal: u16,
  pub semimajor_axis_external: u16,
  pub semiminor_axis_external: u16,
  pub overlap_process_option: bool,
}

// A grid of 4 bit peak luminance values, row by row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeakLuminance {
  pub rows: u8,
  pub columns: u8,
  pub values: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BezierCurve {
  pub knee_point_x: u16,
  pub knee_point_y: u16,
  pub anchors: Vec<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Percentile {
  pub percentage: u8,
  pub value: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
  // The area of the second and third window. The first window is the whole picture.
  pub area: Option<ProcessingWindow>,
  pub maxscl: [u32; 3],
  pub average_maxrgb: u32,
  pub distribution: Vec<Percentile>,
  pub fraction_bright_pixels: u16,
  pub bezier_curve: Option<BezierCurve>,
  pub color_saturation_weight: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hdr10Plus {
  pub application_version: u8,
  pub targeted_system_display_maximum_luminance: u32,
  pub targeted_system_display_actual_peak_luminance: Option<PeakLuminance>,
  pub windows: Vec<Window>,
  pub mastering_display_actual_peak_luminance: Option<PeakLuminance>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
  Warning,
  Error,
}

impl Severity {
  pub fn name(self) -> &'static str {
    match self {
      Severity::Warning => "warning",
      Severity::Error => "error",
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
  pub severity: Severity,
  pub code: &'static str,
  pub message: String,
}

fn issue(severity: Severity, code: &'static str, message: impl Into<String>) -> Issue {
  Issue {
    severity,
    code,
    message: message.into(),
  }
}

// What kind of T.35 metadata an OBU holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum T35Kind {
  Hdr10Plus,
  DolbyVision,
  // Close enough to HDR10+ that one of its identifying codes is probably wrong.
  BrokenHdr10Plus(&'static str),
  Other,
}

fn header_fields(payload: &[u8]) -> Option<(u16, u16, u8)> {
  let provider = u16::from_be_bytes([*payload.first()?, *payload.get(1)?]);
  let oriented = u16::from_be_bytes([*payload.get(2)?, *payload.get(3)?]);
  Some((provider, oriented, *payload.get(4)?))
}

pub fn classify(t35: &ItuTT35) -> T35Kind {
  let Some((provider, oriented, application)) = header_fields(&t35.payload) else {
    return T35Kind::Other;
  };
  let country = t35.country_code == T35_COUNTRY_US;
  if country && provider == T35_PROVIDER_DOLBY {
    return T35Kind::DolbyVision;
  }
  let checks = [
    (country, "country_code"),
    (provider == T35_PROVIDER_SAMSUNG, "provider_code"),
    (oriented == PROVIDER_ORIENTED_CODE, "provider_oriented_code"),
    (application == APPLICATION_IDENTIFIER, "application_identifier"),
  ];
  let wrong: Vec<&'static str> = checks.iter().filter(|(ok, _)| !ok).map(|(_, name)| *name).collect();
  match wrong.as_slice() {
    [] => T35Kind::Hdr10Plus,
    [one] => T35Kind::BrokenHdr10Plus(one),
    _ => T35Kind::Other,
  }
}

// HDR10+ written with another metadata type than ITU-T T.35.
pub fn misplaced_type(metadata: &Metadata) -> bool {
  match metadata {
    Metadata::Other { data, .. } => {
      data.len() > 6
        && data[0] == T35_COUNTRY_US
        && header_fields(&data[1..]) == Some((T35_PROVIDER_SAMSUNG, PROVIDER_ORIENTED_CODE, APPLICATION_IDENTIFIER))
    }
    _ => false,
  }
}

fn read_peak_luminance(r: &mut BitReader) -> rabbit_av1::Result<PeakLuminance> {
  let rows = r.bits(5)? as u8;
  let columns = r.bits(5)? as u8;
  let mut values = Vec::with_capacity(rows as usize * columns as usize);
  for _ in 0..rows as usize * columns as usize {
    values.push(r.bits(4)? as u8);
  }
  Ok(PeakLuminance { rows, columns, values })
}

fn write_peak_luminance(w: &mut BitWriter, peak: &PeakLuminance) {
  w.bits(peak.rows as u32, 5);
  w.bits(peak.columns as u32, 5);
  for &value in &peak.values {
    w.bits(value as u32, 4);
  }
}

impl Hdr10Plus {
  // Reads the T.35 payload, which starts with the terminal provider code.
  pub fn parse(payload: &[u8]) -> Result<Hdr10Plus> {
    Self::read(payload).map_err(|error| fail("invalid_hdr10plus", format!("the HDR10+ metadata is damaged: {error}")))
  }

  fn read(payload: &[u8]) -> rabbit_av1::Result<Hdr10Plus> {
    let mut r = BitReader::new(payload);
    r.skip(16 + 16 + 8)?;
    let application_version = r.bits(8)? as u8;
    let num_windows = r.bits(2)? as usize;
    let mut windows: Vec<Window> = (0..num_windows).map(|_| Window::default()).collect();
    for window in windows.iter_mut().skip(1) {
      window.area = Some(ProcessingWindow {
        upper_left: [r.bits(16)? as u16, r.bits(16)? as u16],
        lower_right: [r.bits(16)? as u16, r.bits(16)? as u16],
        center_of_ellipse: [r.bits(16)? as u16, r.bits(16)? as u16],
        rotation_angle: r.bits(8)? as u8,
        semimajor_axis_internal: r.bits(16)? as u16,
        semimajor_axis_external: r.bits(16)? as u16,
        semiminor_axis_external: r.bits(16)? as u16,
        overlap_process_option: r.bit()?,
      });
    }
    let targeted_system_display_maximum_luminance = r.bits(27)?;
    let targeted_system_display_actual_peak_luminance = if r.bit()? { Some(read_peak_luminance(&mut r)?) } else { None };
    for window in &mut windows {
      for value in &mut window.maxscl {
        *value = r.bits(17)?;
      }
      window.average_maxrgb = r.bits(17)?;
      let count = r.bits(4)?;
      for _ in 0..count {
        window.distribution.push(Percentile {
          percentage: r.bits(7)? as u8,
          value: r.bits(17)?,
        });
      }
      window.fraction_bright_pixels = r.bits(10)? as u16;
    }
    let mastering_display_actual_peak_luminance = if r.bit()? { Some(read_peak_luminance(&mut r)?) } else { None };
    for window in &mut windows {
      if r.bit()? {
        let knee_point_x = r.bits(12)? as u16;
        let knee_point_y = r.bits(12)? as u16;
        let count = r.bits(4)?;
        let mut anchors = Vec::with_capacity(count as usize);
        for _ in 0..count {
          anchors.push(r.bits(10)? as u16);
        }
        window.bezier_curve = Some(BezierCurve {
          knee_point_x,
          knee_point_y,
          anchors,
        });
      }
      if r.bit()? {
        window.color_saturation_weight = Some(r.bits(6)? as u8);
      }
    }
    Ok(Hdr10Plus {
      application_version,
      targeted_system_display_maximum_luminance,
      targeted_system_display_actual_peak_luminance,
      windows,
      mastering_display_actual_peak_luminance,
    })
  }

  // Writes the T.35 payload, starting with the terminal provider code.
  pub fn encode(&self) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(T35_PROVIDER_SAMSUNG as u32, 16);
    w.bits(PROVIDER_ORIENTED_CODE as u32, 16);
    w.bits(APPLICATION_IDENTIFIER as u32, 8);
    w.bits(self.application_version as u32, 8);
    w.bits(self.windows.len() as u32, 2);
    for window in self.windows.iter().skip(1) {
      let area = window.area.clone().unwrap_or_default();
      for value in [
        area.upper_left[0],
        area.upper_left[1],
        area.lower_right[0],
        area.lower_right[1],
        area.center_of_ellipse[0],
        area.center_of_ellipse[1],
      ] {
        w.bits(value as u32, 16);
      }
      w.bits(area.rotation_angle as u32, 8);
      w.bits(area.semimajor_axis_internal as u32, 16);
      w.bits(area.semimajor_axis_external as u32, 16);
      w.bits(area.semiminor_axis_external as u32, 16);
      w.bit(area.overlap_process_option);
    }
    w.bits(self.targeted_system_display_maximum_luminance, 27);
    w.bit(self.targeted_system_display_actual_peak_luminance.is_some());
    if let Some(peak) = &self.targeted_system_display_actual_peak_luminance {
      write_peak_luminance(&mut w, peak);
    }
    for window in &self.windows {
      for value in window.maxscl {
        w.bits(value, 17);
      }
      w.bits(window.average_maxrgb, 17);
      w.bits(window.distribution.len() as u32, 4);
      for percentile in &window.distribution {
        w.bits(percentile.percentage as u32, 7);
        w.bits(percentile.value, 17);
      }
      w.bits(window.fraction_bright_pixels as u32, 10);
    }
    w.bit(self.mastering_display_actual_peak_luminance.is_some());
    if let Some(peak) = &self.mastering_display_actual_peak_luminance {
      write_peak_luminance(&mut w, peak);
    }
    for window in &self.windows {
      w.bit(window.bezier_curve.is_some());
      if let Some(curve) = &window.bezier_curve {
        w.bits(curve.knee_point_x as u32, 12);
        w.bits(curve.knee_point_y as u32, 12);
        w.bits(curve.anchors.len() as u32, 4);
        for &anchor in &curve.anchors {
          w.bits(anchor as u32, 10);
        }
      }
      w.bit(window.color_saturation_weight.is_some());
      if let Some(weight) = window.color_saturation_weight {
        w.bits(weight as u32, 6);
      }
    }
    w.align();
    w.finish()
  }

  pub fn to_metadata(&self) -> Metadata {
    Metadata::ItuTT35(ItuTT35 {
      country_code: T35_COUNTRY_US,
      country_code_extension: None,
      payload: self.encode(),
    })
  }

  // The window that covers the whole picture.
  pub fn main(&self) -> Option<&Window> {
    self.windows.first()
  }

  // HDR10+ profile A has no tone mapping curve, profile B has one for a target display.
  pub fn profile(&self) -> Option<char> {
    let window = self.main()?;
    let target = self.targeted_system_display_maximum_luminance;
    match &window.bezier_curve {
      None if target == 0 => Some('A'),
      Some(curve) if target > 0 && !curve.anchors.is_empty() => Some('B'),
      _ => None,
    }
  }

  // Everything ST 2094-40 and CTA-861 forbid for application version 1, as errors, and what
  // displays may handle badly, as warnings.
  pub fn validate(&self) -> Vec<Issue> {
    use Severity::{Error, Warning};
    let mut issues = Vec::new();
    if self.application_version != APPLICATION_VERSION {
      issues.push(issue(
        Error,
        "application_mode",
        format!("application mode is {}, HDR10+ uses 1", self.application_version),
      ));
    }
    if self.windows.len() != 1 {
      issues.push(issue(
        Error,
        "num_windows",
        format!("{} processing windows, HDR10+ uses 1", self.windows.len()),
      ));
    }
    if self.targeted_system_display_actual_peak_luminance.is_some() {
      issues.push(issue(
        Error,
        "targeted_display_peak_luminance",
        "targeted_system_display_actual_peak_luminance_flag is set, which HDR10+ does not allow",
      ));
    }
    if self.mastering_display_actual_peak_luminance.is_some() {
      issues.push(issue(
        Error,
        "mastering_display_peak_luminance",
        "mastering_display_actual_peak_luminance_flag is set, which HDR10+ does not allow",
      ));
    }
    let target = self.targeted_system_display_maximum_luminance;
    if target > 10_000 {
      issues.push(issue(
        Error,
        "target_luminance",
        format!("the targeted display maximum luminance is {target} cd/m², above 10000"),
      ));
    }
    for window in self.windows.iter().take(1) {
      if window.color_saturation_weight.is_some() {
        issues.push(issue(
          Error,
          "color_saturation_mapping",
          "color_saturation_mapping_flag is set, which HDR10+ does not allow",
        ));
      }
      let values = window
        .maxscl
        .iter()
        .chain([&window.average_maxrgb])
        .chain(window.distribution.iter().map(|percentile| &percentile.value));
      if let Some(value) = values.clone().find(|&&value| value > MAX_VALUE) {
        issues.push(issue(
          Error,
          "value_range",
          format!("a brightness value is {value}, above the limit of 100000 (10000 cd/m²)"),
        ));
      }
      let percentages: Vec<u8> = window.distribution.iter().map(|percentile| percentile.percentage).collect();
      if percentages != PERCENTAGES_9 && percentages != PERCENTAGES_10 {
        issues.push(issue(
          Error,
          "percentages",
          format!("the distribution covers {percentages:?}, HDR10+ uses {PERCENTAGES_9:?} or {PERCENTAGES_10:?}"),
        ));
      } else {
        // The 5% and 10% entries are not part of the distribution.
        let ordered: Vec<u32> = window
          .distribution
          .iter()
          .filter(|percentile| percentile.percentage != 5 && percentile.percentage != 10)
          .map(|percentile| percentile.value)
          .collect();
        if ordered.windows(2).any(|pair| pair[1] < pair[0]) {
          issues.push(issue(
            Warning,
            "percentiles_order",
            "the brightness distribution decreases from one percentile to the next",
          ));
        }
      }
      let peak = window.maxscl.iter().copied().max().unwrap_or(0);
      if window.average_maxrgb > peak {
        issues.push(issue(
          Warning,
          "average_above_peak",
          format!(
            "the average brightness {} is above the brightest component {}",
            nits(window.average_maxrgb),
            nits(peak)
          ),
        ));
      }
      if let Some(curve) = &window.bezier_curve
        && curve.anchors.len() > 9
      {
        issues.push(issue(
          Error,
          "bezier_anchors",
          format!("the tone mapping curve has {} anchors, at most 9 are allowed", curve.anchors.len()),
        ));
      }
      let curve = window.bezier_curve.is_some();
      if curve && target == 0 {
        issues.push(issue(
          Warning,
          "profile",
          "a tone mapping curve without a targeted display luminance fits neither profile A nor B",
        ));
      } else if !curve && target != 0 {
        issues.push(issue(
          Warning,
          "profile",
          format!("a targeted display luminance of {target} cd/m² without a tone mapping curve fits neither profile A nor B"),
        ));
      }
    }
    issues
  }

  // The brightest component, in cd/m².
  pub fn peak_nits(&self) -> f64 {
    self
      .main()
      .map_or(0.0, |window| window.maxscl.iter().copied().max().unwrap_or(0) as f64 / 10.0)
  }

  pub fn average_nits(&self) -> f64 {
    self.main().map_or(0.0, |window| window.average_maxrgb as f64 / 10.0)
  }

  pub fn percentile_nits(&self, percentage: u8) -> Option<f64> {
    self
      .main()?
      .distribution
      .iter()
      .find(|percentile| percentile.percentage == percentage)
      .map(|percentile| percentile.value as f64 / 10.0)
  }
}

pub fn nits(value: u32) -> String {
  format!("{} cd/m²", value as f64 / 10.0)
}

#[cfg(test)]
pub mod tests {
  use super::*;

  pub fn sample() -> Hdr10Plus {
    let values = [3, 14024, 43, 56, 219, 1036, 2714, 4668, 14445];
    Hdr10Plus {
      application_version: 1,
      targeted_system_display_maximum_luminance: 0,
      targeted_system_display_actual_peak_luminance: None,
      windows: vec![Window {
        maxscl: [17830, 16895, 14252],
        average_maxrgb: 1037,
        distribution: PERCENTAGES_9
          .iter()
          .zip(values)
          .map(|(&percentage, value)| Percentile { percentage, value })
          .collect(),
        ..Window::default()
      }],
      mastering_display_actual_peak_luminance: None,
    }
  }

  #[test]
  fn round_trips_profile_a() {
    let meta = sample();
    let payload = meta.encode();
    assert_eq!(&payload[..6], &[0x00, 0x3c, 0x00, 0x01, 0x04, 0x01]);
    assert_eq!(Hdr10Plus::parse(&payload).unwrap(), meta);
    assert_eq!(meta.profile(), Some('A'));
    assert!(meta.validate().is_empty(), "{:?}", meta.validate());
    let Metadata::ItuTT35(t35) = meta.to_metadata() else { panic!() };
    assert_eq!(classify(&t35), T35Kind::Hdr10Plus);
  }

  #[test]
  fn round_trips_profile_b_and_rare_fields() {
    let mut meta = sample();
    meta.targeted_system_display_maximum_luminance = 400;
    meta.windows[0].bezier_curve = Some(BezierCurve {
      knee_point_x: 17,
      knee_point_y: 64,
      anchors: vec![102, 205, 307, 410, 512, 614, 717, 819, 922],
    });
    assert_eq!(meta.profile(), Some('B'));
    assert!(meta.validate().is_empty(), "{:?}", meta.validate());
    meta.targeted_system_display_actual_peak_luminance = Some(PeakLuminance {
      rows: 2,
      columns: 3,
      values: vec![1, 2, 3, 4, 5, 6],
    });
    meta.windows[0].color_saturation_weight = Some(9);
    meta.windows.push(Window {
      area: Some(ProcessingWindow {
        upper_left: [1, 2],
        rotation_angle: 7,
        overlap_process_option: true,
        ..ProcessingWindow::default()
      }),
      ..meta.windows[0].clone()
    });
    assert_eq!(Hdr10Plus::parse(&meta.encode()).unwrap(), meta);
    let codes: Vec<&str> = meta.validate().iter().map(|issue| issue.code).collect();
    assert!(
      codes.contains(&"num_windows") && codes.contains(&"targeted_display_peak_luminance"),
      "{codes:?}"
    );
    assert!(codes.contains(&"color_saturation_mapping"), "{codes:?}");
  }

  #[test]
  fn reports_broken_values() {
    let mut meta = sample();
    meta.application_version = 0;
    meta.windows[0].maxscl[0] = 100_001;
    meta.windows[0].distribution.pop();
    let codes: Vec<&str> = meta.validate().iter().map(|issue| issue.code).collect();
    assert_eq!(codes, vec!["application_mode", "value_range", "percentages"]);
    assert!(Hdr10Plus::parse(&[0x00, 0x3c, 0x00]).is_err());
  }

  #[test]
  fn damaged_payloads_parse_or_fail_cleanly() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = |limit: usize| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      (state % limit.max(1) as u64) as usize
    };
    let base = sample().encode();
    for _ in 0..20_000 {
      let mut data = base.clone();
      match next(3) {
        0 => {
          for _ in 0..1 + next(4) {
            let at = next(data.len());
            data[at] ^= 1 << next(8);
          }
        }
        1 => data.truncate(next(data.len())),
        _ => data = (0..next(64)).map(|_| next(256) as u8).collect(),
      }
      if let Ok(meta) = Hdr10Plus::parse(&data) {
        let _ = meta.validate();
        assert_eq!(Hdr10Plus::parse(&meta.encode()).unwrap(), meta);
      }
    }
  }

  #[test]
  fn classifies_t35() {
    let t35 = |country, payload: Vec<u8>| ItuTT35 {
      country_code: country,
      country_code_extension: None,
      payload,
    };
    assert_eq!(classify(&t35(0xb5, vec![0x00, 0x3b, 0x00, 0x00, 0x08])), T35Kind::DolbyVision);
    assert_eq!(
      classify(&t35(0xb4, vec![0x00, 0x3c, 0x00, 0x01, 0x04, 0x01])),
      T35Kind::BrokenHdr10Plus("country_code")
    );
    assert_eq!(
      classify(&t35(0xb5, vec![0x00, 0x3c, 0x00, 0x02, 0x04, 0x01])),
      T35Kind::BrokenHdr10Plus("provider_oriented_code")
    );
    assert_eq!(classify(&t35(0xb5, vec![0x00, 0x31, 0x47, 0x41, 0x39])), T35Kind::Other);
  }
}
