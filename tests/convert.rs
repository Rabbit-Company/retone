// Converting Dolby Vision to HDR10+, with Dolby Vision metadata made by the dolby_vision crate
// and put into HEVC by x265 and into AV1 here.

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::extension_metadata::blocks::{ExtMetadataBlock, ExtMetadataBlockLevel1, ExtMetadataBlockLevel6};
use dolby_vision::rpu::generate::{GenerateConfig, VideoShot};
use hevc_parser::HevcParser;
use rabbit_av1::metadata::Metadata;
use rabbit_av1::{Input, Output};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

const FRAMES: usize = 48;
// The picture changes at frame 24. The Dolby Vision shots start elsewhere, so the result
// shows whose scenes were used.
const SHOTS: [(usize, usize); 2] = [(0, 20), (20, 28)];

fn retone(args: &[&str]) -> (bool, Vec<Value>) {
  let output = Command::new(env!("CARGO_BIN_EXE_retone"))
    .args(["--progress", "json"])
    .args(args)
    .output()
    .expect("retone did not start");
  let events = String::from_utf8_lossy(&output.stdout)
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
  (output.status.success(), events)
}

fn event<'a>(events: &'a [Value], name: &str) -> &'a Value {
  events
    .iter()
    .find(|event| event["event"] == name)
    .unwrap_or_else(|| panic!("no {name} event in {events:?}"))
}

fn ffmpeg(args: &[&str]) -> bool {
  Command::new("ffmpeg")
    .args(["-v", "error", "-nostdin", "-y"])
    .args(args)
    .status()
    .is_ok_and(|status| status.success())
}

fn workspace(name: &str) -> PathBuf {
  let dir = std::env::temp_dir().join(format!("retone-convert-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir
}

fn path(dir: &Path, name: &str) -> String {
  dir.join(name).to_string_lossy().into_owned()
}

fn config(frames: usize) -> GenerateConfig {
  let mut shots: Vec<VideoShot> = SHOTS
    .iter()
    .enumerate()
    .map(|(index, &(start, duration))| VideoShot {
      start,
      duration,
      metadata_blocks: vec![ExtMetadataBlock::Level1(ExtMetadataBlockLevel1::new(
        0,
        2900 + index as u16 * 200,
        1500,
      ))],
      ..VideoShot::default()
    })
    .collect();
  shots.last_mut().unwrap().duration -= FRAMES - frames;
  GenerateConfig {
    length: frames,
    shots,
    level6: Some(ExtMetadataBlockLevel6 {
      max_display_mastering_luminance: 1000,
      min_display_mastering_luminance: 1,
      max_content_light_level: 900,
      max_frame_average_light_level: 200,
    }),
    ..GenerateConfig::default()
  }
}

// A PQ source of two scenes, an RPU file for it, and the source in AV1 and HEVC with
// Dolby Vision. None without ffmpeg, SVT-AV1 and libx265 with Dolby Vision support.
struct Sample {
  source: String,
  rpu: String,
  av1: String,
  hevc: Option<String>,
}

fn sample(dir: &Path) -> Option<Sample> {
  let source = path(dir, "source.mkv");
  let filter = "testsrc2=size=192x128:rate=24:duration=1[a];smptehdbars=size=192x128:rate=24:duration=1[b];[a][b]concat=n=2,format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc";
  if !ffmpeg(&["-f", "lavfi", "-i", filter, "-c:v", "ffv1", &source]) {
    return None;
  }
  let rpu = path(dir, "RPU.bin");
  config(FRAMES).write_rpus(&rpu).unwrap();

  let plain = path(dir, "plain.ivf");
  let params = "color-primaries=9:transfer-characteristics=16:matrix-coefficients=9";
  if !ffmpeg(&[
    "-i",
    &source,
    "-c:v",
    "libsvtav1",
    "-preset",
    "12",
    "-crf",
    "40",
    "-svtav1-params",
    params,
    &plain,
  ]) {
    return None;
  }
  // Every temporal unit gets the RPU of its frame, after the sequence header.
  let rpus = config(FRAMES).generate_rpu_list().unwrap();
  let av1 = path(dir, "dolby.ivf");
  let mut input = Input::open(Path::new(&plain), "ffmpeg", None).unwrap();
  let mut output = Output::create(Path::new(&av1), Path::new(&plain), &input.reader, "ffmpeg").unwrap();
  let mut index = 0;
  while let Some(mut packet) = input.reader.next_packet().unwrap() {
    let complete = rpus[index].write_av1_rpu_metadata_obu_t35_complete().unwrap();
    let Metadata::ItuTT35(t35) = Metadata::parse(&[&[4u8][..], &complete, &[0x80]].concat()).unwrap() else {
      panic!("not T.35");
    };
    let at = packet.metadata_index().unwrap();
    packet.insert_obu(at, &Metadata::ItuTT35(t35).to_obu(None)).unwrap();
    output.write(&packet).unwrap();
    index += 1;
  }
  input.finish().unwrap();
  output.finish().unwrap().commit().unwrap();
  assert_eq!(index, FRAMES);

  // HEVC with B-frames, so frames are coded in another order than they are shown. Every
  // coded frame gets the RPU of the frame it shows.
  let coded = path(dir, "coded.hevc");
  let made = ffmpeg(&[
    "-i",
    &source,
    "-c:v",
    "libx265",
    "-preset",
    "ultrafast",
    "-x265-params",
    "bframes=3:keyint=24:hdr10=1:repeat-headers=1",
    &coded,
  ]);
  let hevc = made.then(|| {
    let raw = std::fs::read(&coded).unwrap();
    let hevc = path(dir, "dolby.hevc");
    std::fs::write(&hevc, with_rpus(&raw, &rpus)).unwrap();
    hevc
  });
  if hevc.is_none() {
    eprintln!("libx265 is missing, HEVC is not tested");
  }
  Some(Sample { source, rpu, av1, hevc })
}

// Puts an RPU NAL unit after the last NAL unit of every coded frame, the RPU of the frame
// in display order.
fn with_rpus(raw: &[u8], rpus: &[DoviRpu]) -> Vec<u8> {
  let mut parser = HevcParser::default();
  let mut offsets = Vec::new();
  parser.get_offsets(raw, &mut offsets);
  let last = *offsets.last().unwrap();
  parser.split_nals(raw, &offsets, last, true).unwrap();
  parser.finish();
  let mut frames: Vec<(u64, u64, usize)> = parser
    .ordered_frames()
    .iter()
    .map(|frame| {
      let end = frame.nals.iter().map(|nal| nal.end).max().unwrap();
      (frame.decoded_number, frame.presentation_number, end)
    })
    .collect();
  assert_eq!(frames.len(), FRAMES);
  assert!(frames.iter().any(|&(decoded, shown, _)| decoded != shown), "no B-frames");
  frames.sort();
  let mut out = Vec::with_capacity(raw.len() + FRAMES * 200);
  let mut position = 0;
  for (_, shown, end) in frames {
    out.extend_from_slice(&raw[position..end]);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend(rpus[shown as usize].write_hevc_unspec62_nalu().unwrap());
    position = end;
  }
  out.extend_from_slice(&raw[position..]);
  out
}

fn scene_starts(file: &str) -> Value {
  let json: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
  json["SceneInfoSummary"]["SceneFirstFrameIndex"].clone()
}

#[test]
fn converts_with_dolby_vision_shots() {
  let dir = workspace("shots");
  let Some(sample) = sample(&dir) else {
    eprintln!("skipped: ffmpeg with libsvtav1 is needed");
    return;
  };
  let output = path(&dir, "hdr10plus.json");
  let mut inputs = vec![sample.av1.clone()];
  if let Some(hevc) = &sample.hevc {
    inputs.push(hevc.clone());
    // ffmpeg cannot give raw HEVC with B-frames timestamps, mkvmerge can.
    let mkv = path(&dir, "dolby.mkv");
    let muxed = Command::new("mkvmerge")
      .args(["-q", "-o", &mkv, "--default-duration", "0:24fps", hevc])
      .status()
      .is_ok_and(|status| status.success());
    if muxed {
      let mp4 = path(&dir, "dolby.mp4");
      assert!(ffmpeg(&["-i", &mkv, "-c", "copy", &mp4]));
      inputs.extend([mkv, mp4]);
    } else {
      eprintln!("mkvmerge is missing, HEVC in containers is not tested");
    }
  }
  for input in &inputs {
    let (ok, events) = retone(&["convert", input, "-o", &output, "-y"]);
    assert!(ok, "{input}: {events:?}");
    assert_eq!(event(&events, "start")["frames"], FRAMES, "{input}");
    let summary = event(&events, "summary");
    assert_eq!(summary["frames"], FRAMES, "{input}");
    assert_eq!(summary["scene_source"], "dolby_vision_shots", "{input}");
    assert_eq!(summary["dolby_vision_profile"], 8, "{input}");
    assert_eq!(summary["dolby_vision_level6"]["max_cll"], 900, "{input}");
    assert_eq!(scene_starts(&output), serde_json::json!([0, 20]), "{input}");
  }

  // An RPU file needs the video it belongs to.
  let (ok, events) = retone(&["convert", &sample.rpu, "-o", &output, "-y"]);
  assert!(!ok);
  assert_eq!(event(&events, "error")["code"], "invalid_argument");
  let (ok, events) = retone(&["convert", &sample.rpu, "--video", &sample.source, "-o", &output, "-y"]);
  assert!(ok, "{events:?}");
  assert_eq!(scene_starts(&output), serde_json::json!([0, 20]));

  // Measuring the same video finds the cut in the picture instead.
  let measured = path(&dir, "measured.json");
  let (ok, _) = retone(&["measure", &sample.source, "-o", &measured]);
  assert!(ok);
  assert_eq!(scene_starts(&measured), serde_json::json!([0, 24]));
}

#[test]
fn refuses_metadata_for_another_frame_count() {
  let dir = workspace("count");
  let Some(sample) = sample(&dir) else {
    eprintln!("skipped: ffmpeg with libsvtav1 is needed");
    return;
  };
  let short = path(&dir, "short.bin");
  config(FRAMES - 1).write_rpus(&short).unwrap();
  let output = path(&dir, "out.json");
  let (ok, events) = retone(&["convert", &short, "--video", &sample.source, "-o", &output]);
  assert!(!ok);
  assert_eq!(event(&events, "error")["code"], "frame_count_mismatch");
  assert!(!Path::new(&output).exists());
}
