// HDR10+ metadata as JSON, in the layout hdr10plus_tool reads and writes: one SceneInfo entry
// per frame, in display order.

use crate::hdr10plus::{APPLICATION_VERSION, BezierCurve, Hdr10Plus, Percentile, Window};
use crate::report::{Coded, fail};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Root {
  #[serde(rename = "JSONInfo")]
  pub info: Info,
  #[serde(rename = "SceneInfo")]
  pub scene_info: Vec<Frame>,
  #[serde(rename = "SceneInfoSummary", default)]
  pub summary: Summary,
  #[serde(rename = "ToolInfo", default)]
  pub tool: ToolInfo,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Info {
  #[serde(rename = "HDR10plusProfile", default)]
  pub profile: String,
  #[serde(rename = "Version", default)]
  pub version: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct Frame {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub bezier_curve_data: Option<BezierCurveData>,
  pub luminance_parameters: LuminanceParameters,
  pub number_of_windows: u8,
  pub targeted_system_display_maximum_luminance: u32,
  #[serde(default)]
  pub scene_frame_index: usize,
  #[serde(default)]
  pub scene_id: usize,
  #[serde(default)]
  pub sequence_frame_index: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct BezierCurveData {
  pub anchors: Vec<u16>,
  pub knee_point_x: u16,
  pub knee_point_y: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct LuminanceParameters {
  #[serde(rename = "AverageRGB")]
  pub average_rgb: u32,
  pub luminance_distributions: LuminanceDistributions,
  pub max_scl: Vec<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct LuminanceDistributions {
  pub distribution_index: Vec<u8>,
  pub distribution_values: Vec<u32>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Summary {
  pub scene_first_frame_index: Vec<usize>,
  pub scene_frame_numbers: Vec<usize>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ToolInfo {
  pub tool: String,
  pub version: String,
}

impl Frame {
  fn to_metadata(&self, index: usize) -> Result<Hdr10Plus> {
    let luminance = &self.luminance_parameters;
    let distributions = &luminance.luminance_distributions;
    let [r, g, b] = luminance.max_scl[..] else {
      return Err(fail(
        "invalid_metadata",
        format!("frame {index}: MaxScl has {} values instead of 3", luminance.max_scl.len()),
      ));
    };
    if distributions.distribution_index.len() != distributions.distribution_values.len() {
      return Err(fail(
        "invalid_metadata",
        format!("frame {index}: DistributionIndex and DistributionValues differ in length"),
      ));
    }
    if distributions.distribution_index.len() > 15 {
      return Err(fail("invalid_metadata", format!("frame {index}: more than 15 distribution values")));
    }
    if let Some(curve) = &self.bezier_curve_data
      && curve.anchors.len() > 15
    {
      return Err(fail("invalid_metadata", format!("frame {index}: more than 15 Bezier anchors")));
    }
    // ST 2094-40 allows 1 to 3 windows, and only the first comes without an area that JSON cannot hold.
    if self.number_of_windows != 1 {
      return Err(fail(
        "invalid_metadata",
        format!("frame {index}: {} windows, only 1 can be written", self.number_of_windows),
      ));
    }
    let fits = |value: u32, bits: u32| value < 1 << bits;
    let values_fit = [r, g, b, luminance.average_rgb]
      .into_iter()
      .chain(distributions.distribution_values.iter().copied())
      .all(|value| fits(value, 17))
      && distributions.distribution_index.iter().all(|&index| fits(index as u32, 7))
      && fits(self.targeted_system_display_maximum_luminance, 27)
      && self.bezier_curve_data.as_ref().is_none_or(|curve| {
        fits(curve.knee_point_x as u32, 12)
          && fits(curve.knee_point_y as u32, 12)
          && curve.anchors.iter().all(|&anchor| fits(anchor as u32, 10))
      });
    if !values_fit {
      return Err(fail(
        "invalid_metadata",
        format!("frame {index}: a value is too large for its field in the bitstream"),
      ));
    }
    Ok(Hdr10Plus {
      application_version: APPLICATION_VERSION,
      targeted_system_display_maximum_luminance: self.targeted_system_display_maximum_luminance,
      targeted_system_display_actual_peak_luminance: None,
      windows: vec![Window {
        area: None,
        maxscl: [r, g, b],
        average_maxrgb: luminance.average_rgb,
        distribution: distributions
          .distribution_index
          .iter()
          .zip(&distributions.distribution_values)
          .map(|(&percentage, &value)| Percentile { percentage, value })
          .collect(),
        fraction_bright_pixels: 0,
        bezier_curve: self.bezier_curve_data.as_ref().map(|curve| BezierCurve {
          knee_point_x: curve.knee_point_x,
          knee_point_y: curve.knee_point_y,
          anchors: curve.anchors.clone(),
        }),
        color_saturation_weight: None,
      }],
      mastering_display_actual_peak_luminance: None,
    })
  }

  fn from_metadata(meta: &Hdr10Plus) -> Frame {
    let window = meta.main().cloned().unwrap_or_default();
    Frame {
      bezier_curve_data: window.bezier_curve.as_ref().map(|curve| BezierCurveData {
        anchors: curve.anchors.clone(),
        knee_point_x: curve.knee_point_x,
        knee_point_y: curve.knee_point_y,
      }),
      luminance_parameters: LuminanceParameters {
        average_rgb: window.average_maxrgb,
        luminance_distributions: LuminanceDistributions {
          distribution_index: window.distribution.iter().map(|percentile| percentile.percentage).collect(),
          distribution_values: window.distribution.iter().map(|percentile| percentile.value).collect(),
        },
        max_scl: window.maxscl.to_vec(),
      },
      number_of_windows: meta.windows.len() as u8,
      targeted_system_display_maximum_luminance: meta.targeted_system_display_maximum_luminance,
      scene_frame_index: 0,
      scene_id: 0,
      sequence_frame_index: None,
    }
  }

  // The parts that make a frame belong to the same scene as the one before it.
  fn same_scene(&self, other: &Frame) -> bool {
    self.bezier_curve_data == other.bezier_curve_data
      && self.luminance_parameters == other.luminance_parameters
      && self.number_of_windows == other.number_of_windows
      && self.targeted_system_display_maximum_luminance == other.targeted_system_display_maximum_luminance
  }
}

// The metadata of every frame in display order.
pub struct MetadataFile {
  pub frames: Vec<Hdr10Plus>,
}

pub fn parse(text: &str) -> Result<MetadataFile> {
  let root: Root = serde_json::from_str(text)
    .context("not HDR10+ JSON metadata")
    .code("invalid_metadata")?;
  let mut frames = root.scene_info;
  // Entries are in display order. When they carry their frame numbers, those decide.
  if frames.iter().all(|frame| frame.sequence_frame_index.is_some()) {
    frames.sort_by_key(|frame| frame.sequence_frame_index);
    for (index, frame) in frames.iter().enumerate() {
      if frame.sequence_frame_index != Some(index) {
        return Err(fail(
          "invalid_metadata",
          format!("SequenceFrameIndex skips or repeats frame {index}"),
        ));
      }
    }
  }
  let mut metadata = Vec::with_capacity(frames.len());
  for (index, frame) in frames.iter().enumerate() {
    metadata.push(frame.to_metadata(index)?);
  }
  Ok(MetadataFile { frames: metadata })
}

pub fn read(path: &Path) -> Result<MetadataFile> {
  let text = std::fs::read_to_string(path)
    .with_context(|| format!("failed to read {}", path.display()))
    .code("metadata_not_found")?;
  parse(&text).with_context(|| format!("invalid HDR10+ metadata {}", path.display()))
}

pub fn to_json(frames: &[Hdr10Plus]) -> String {
  let mut list: Vec<Frame> = frames.iter().map(Frame::from_metadata).collect();
  let mut scene_starts = Vec::new();
  let mut scene_id = 0;
  let mut scene_frame = 0;
  for index in 0..list.len() {
    if index > 0 && !list[index].same_scene(&list[index - 1]) {
      scene_id += 1;
      scene_frame = 0;
    }
    if scene_frame == 0 {
      scene_starts.push(index);
    }
    let frame = &mut list[index];
    frame.scene_id = scene_id;
    frame.scene_frame_index = scene_frame;
    frame.sequence_frame_index = Some(index);
    scene_frame += 1;
  }
  let lengths = scene_starts
    .iter()
    .enumerate()
    .map(|(i, &start)| scene_starts.get(i + 1).copied().unwrap_or(list.len()) - start)
    .collect();
  let profiles: Vec<Option<char>> = frames.iter().map(Hdr10Plus::profile).collect();
  let profile = match profiles.first() {
    Some(Some(first)) if profiles.iter().all(|profile| *profile == Some(*first)) => first.to_string(),
    _ => "N/A".to_string(),
  };
  let root = Root {
    info: Info {
      profile,
      version: format!("{}.0", frames.first().map_or(APPLICATION_VERSION, |meta| meta.application_version)),
    },
    scene_info: list,
    summary: Summary {
      scene_first_frame_index: scene_starts,
      scene_frame_numbers: lengths,
    },
    tool: ToolInfo {
      tool: "retone".to_string(),
      version: env!("CARGO_PKG_VERSION").to_string(),
    },
  };
  let mut text = serde_json::to_string_pretty(&root).unwrap_or_default();
  text.push('\n');
  text
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::hdr10plus::tests::sample;

  #[test]
  fn round_trips_and_finds_scenes() {
    let mut other = sample();
    other.windows[0].average_maxrgb = 500;
    let frames = vec![sample(), sample(), other.clone(), other, sample()];
    let text = to_json(&frames);
    let root: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(root["JSONInfo"]["HDR10plusProfile"], "A");
    assert_eq!(root["SceneInfoSummary"]["SceneFirstFrameIndex"], serde_json::json!([0, 2, 4]));
    assert_eq!(root["SceneInfoSummary"]["SceneFrameNumbers"], serde_json::json!([2, 2, 1]));
    assert_eq!(root["SceneInfo"][3]["SceneFrameIndex"], 1);
    assert!(root["SceneInfo"][0].get("BezierCurveData").is_none());
    let parsed = parse(&text).unwrap();
    assert_eq!(parsed.frames, frames);
  }

  #[test]
  fn reads_hdr10plus_tool_files() {
    let text = r#"{"JSONInfo":{"HDR10plusProfile":"B","Version":"1.0"},
      "SceneInfo":[
        {"BezierCurveData":{"Anchors":[1,2,3],"KneePointX":5,"KneePointY":6},
         "LuminanceParameters":{"AverageRGB":7,"LuminanceDistributions":{"DistributionIndex":[1,5],"DistributionValues":[8,9]},"MaxScl":[10,11,12]},
         "NumberOfWindows":1,"TargetedSystemDisplayMaximumLuminance":400,"SceneFrameIndex":1,"SceneId":0,"SequenceFrameIndex":1},
        {"LuminanceParameters":{"AverageRGB":1,"LuminanceDistributions":{"DistributionIndex":[],"DistributionValues":[]},"MaxScl":[1,2,3]},
         "NumberOfWindows":1,"TargetedSystemDisplayMaximumLuminance":0,"SceneFrameIndex":0,"SceneId":0,"SequenceFrameIndex":0}],
      "SceneInfoSummary":{"SceneFirstFrameIndex":[0],"SceneFrameNumbers":[2]},
      "ToolInfo":{"Tool":"hdr10plus_tool","Version":"1.7.2"}}"#;
    let parsed = parse(text).unwrap();
    assert_eq!(parsed.frames[0].windows[0].maxscl, [1, 2, 3]);
    let curve = parsed.frames[1].windows[0].bezier_curve.as_ref().unwrap();
    assert_eq!((curve.knee_point_x, curve.anchors.len()), (5, 3));
  }

  #[test]
  fn rejects_what_cannot_be_written() {
    let base = to_json(&[sample()]);
    let broken = base.replace("\"NumberOfWindows\": 1", "\"NumberOfWindows\": 2");
    assert!(parse(&broken).is_err());
    let huge = base.replace("17830", "200000");
    assert!(parse(&huge).is_err());
    assert!(parse("{}").is_err());
  }
}
