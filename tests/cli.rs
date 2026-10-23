use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn retone(args: &[&str]) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_retone"))
    .args(args)
    .output()
    .expect("retone did not start")
}

// Runs retone with JSON progress and returns its events.
fn events(args: &[&str]) -> (bool, Vec<Value>) {
  let mut all = vec!["--progress", "json"];
  all.extend_from_slice(args);
  let output = retone(&all);
  let events = String::from_utf8_lossy(&output.stdout)
    .lines()
    .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not JSON: {line}")))
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
  let dir = std::env::temp_dir().join(format!("retone-test-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir
}

fn path(dir: &Path, name: &str) -> String {
  dir.join(name).to_string_lossy().into_owned()
}

// The MD5 of the decoded pictures.
fn picture(file: &str) -> String {
  let output = Command::new("ffmpeg")
    .args([
      "-v", "error", "-nostdin", "-c:v", "libdav1d", "-i", file, "-map", "0:V:0", "-f", "md5", "-",
    ])
    .output()
    .unwrap();
  String::from_utf8_lossy(&output.stdout).into_owned()
}

const PQ: [&str; 6] = ["-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc"];

// A lossless PQ source with two scenes, and AV1 encodes of it. None without ffmpeg, SVT-AV1 or libaom.
fn sample(dir: &Path) -> Option<(String, Vec<String>)> {
  let source = path(dir, "source.mkv");
  let filter = "testsrc2=size=192x128:rate=24:duration=1[a];smptehdbars=size=192x128:rate=24:duration=1[b];[a][b]concat=n=2,format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc";
  let mut args = vec!["-f", "lavfi", "-i", filter, "-c:v", "ffv1"];
  args.extend(PQ);
  args.push(&source);
  if !ffmpeg(&args) {
    return None;
  }
  let svt = path(dir, "svt.ivf");
  let aom = path(dir, "aom.mkv");
  let svt_params = "color-primaries=9:transfer-characteristics=16:matrix-coefficients=9";
  let made_svt = ffmpeg(&[
    "-i",
    &source,
    "-c:v",
    "libsvtav1",
    "-preset",
    "12",
    "-crf",
    "40",
    "-svtav1-params",
    svt_params,
    &svt,
  ]);
  let mut aom_args = vec![
    "-i",
    &source,
    "-c:v",
    "libaom-av1",
    "-cpu-used",
    "8",
    "-crf",
    "40",
    "-lag-in-frames",
    "16",
  ];
  aom_args.extend(PQ);
  aom_args.push(&aom);
  let made_aom = ffmpeg(&aom_args);
  (made_svt && made_aom).then(|| (source, vec![svt, aom]))
}

fn skipped() {
  eprintln!("skipped: ffmpeg with libsvtav1, libaom-av1 and libdav1d is needed");
}

#[test]
fn measure_apply_export_check_and_remove() {
  let dir = workspace("round-trip");
  let Some((source, encodes)) = sample(&dir) else { return skipped() };
  let metadata = path(&dir, "meta.json");
  let (ok, measured) = events(&["measure", &source, "-o", &metadata]);
  assert!(ok, "{measured:?}");
  let start = event(&measured, "start");
  assert_eq!((start["command"].as_str(), start["frames"].as_u64()), (Some("measure"), Some(48)));
  let summary = event(&measured, "summary");
  assert_eq!(summary["frames"], 48);
  assert_eq!(summary["scenes"], 2, "{summary}");
  let written: Value = serde_json::from_str(&std::fs::read_to_string(&metadata).unwrap()).unwrap();
  assert_eq!(written["JSONInfo"]["HDR10plusProfile"], "A");
  assert_eq!(written["SceneInfoSummary"]["SceneFirstFrameIndex"], serde_json::json!([0, 24]));

  for (index, encode) in encodes.iter().enumerate() {
    let extension = Path::new(encode).extension().unwrap().to_str().unwrap();
    let tagged = path(&dir, &format!("tagged{index}.{extension}"));
    let (ok, applied) = events(&["apply", encode, "-m", &metadata, "-o", &tagged, "--verify"]);
    assert!(ok, "{encode}: {applied:?}");
    assert_eq!(event(&applied, "summary")["verified"], true);
    // Every stage reports when it begins, and verifying ends at the frames it decoded.
    assert_eq!(applied[0], serde_json::json!({"event": "start", "command": "apply"}));
    let stages: Vec<&str> = applied
      .iter()
      .filter(|event| event["event"] == "progress")
      .map(|event| event["stage"].as_str().unwrap())
      .collect();
    assert_eq!(stages.first(), Some(&"read"), "{applied:?}");
    assert!(stages.contains(&"write") && stages.contains(&"verify"), "{applied:?}");
    let verify = applied.iter().find(|event| event["stage"] == "verify").unwrap();
    assert_eq!((verify["total"].as_u64(), verify["percent"].as_f64()), (Some(48), Some(20.0)));

    let exported = path(&dir, "exported.json");
    assert!(retone(&["export", &tagged, "-o", &exported, "-y"]).status.success());
    let again: Value = serde_json::from_str(&std::fs::read_to_string(&exported).unwrap()).unwrap();
    assert_eq!(again["SceneInfo"], written["SceneInfo"], "{encode}");

    let (ok, checked) = events(&["check", &tagged]);
    assert!(ok, "{encode}: {checked:?}");
    assert_eq!(event(&checked, "summary")["errors"], 0);

    let (_, info) = events(&["info", &tagged]);
    let info = event(&info, "summary");
    assert_eq!(info["hdr10plus_frames"], 48);
    assert!(info["formats"].as_array().unwrap().contains(&Value::from("HDR10+")));

    let removed = path(&dir, &format!("removed{index}.{extension}"));
    let (ok, result) = events(&["remove", &tagged, "-o", &removed]);
    assert!(ok, "{result:?}");
    // remove does not count the frames first, so it has no total or percent.
    let writing = result.iter().find(|event| event["event"] == "progress").unwrap();
    assert_eq!(
      (writing["stage"].as_str(), &writing["total"], &writing["percent"]),
      (Some("write"), &Value::Null, &Value::Null)
    );
    assert_eq!(event(&result, "summary")["hdr10plus"], 48);
    let (_, info) = events(&["info", &removed]);
    assert_eq!(event(&info, "summary")["hdr10plus_frames"], 0);

    let reference = picture(encode);
    assert!(reference.starts_with("MD5="), "dav1d is missing: {reference}");
    assert_eq!(picture(&tagged), reference);
    assert_eq!(picture(&removed), reference);
  }
}

#[test]
fn apply_refuses_what_it_cannot_write() {
  let dir = workspace("refuse");
  let Some((source, encodes)) = sample(&dir) else { return skipped() };
  let metadata = path(&dir, "meta.json");
  assert!(retone(&["measure", &source, "-o", &metadata]).status.success());
  let output = path(&dir, "out.ivf");

  // One frame too few.
  let mut short: Value = serde_json::from_str(&std::fs::read_to_string(&metadata).unwrap()).unwrap();
  short["SceneInfo"].as_array_mut().unwrap().pop();
  let short_path = path(&dir, "short.json");
  std::fs::write(&short_path, short.to_string()).unwrap();
  let (ok, result) = events(&["apply", &encodes[0], "-m", &short_path, "-o", &output]);
  assert!(!ok);
  assert_eq!(event(&result, "error")["code"], "frame_count_mismatch");

  // A video without colour tags, as some encoders leave it.
  let untagged = path(&dir, "untagged.ivf");
  assert!(ffmpeg(&[
    "-i",
    &encodes[0],
    "-c",
    "copy",
    "-bsf:v",
    "av1_metadata=color_primaries=2:transfer_characteristics=2:matrix_coefficients=2",
    &untagged
  ]));
  let (ok, result) = events(&["apply", &untagged, "-m", &metadata, "-o", &output]);
  assert!(!ok);
  assert_eq!(event(&result, "error")["code"], "not_hdr10");
  let (ok, result) = events(&["apply", &untagged, "-m", &metadata, "-o", &output, "--fix-color", "--verify"]);
  assert!(ok, "{result:?}");
  assert_eq!(event(&result, "summary")["color_fixed"], true);
  let (_, info) = events(&["info", &output]);
  let info = event(&info, "summary");
  assert_eq!(
    (info["color_primaries"].clone(), info["transfer_characteristics"].clone()),
    (9.into(), 16.into())
  );
  assert_eq!(picture(&output), picture(&untagged));

  // A video tagged as BT.709 SDR is a different picture, so only --force-fix-color retags it.
  let sdr = path(&dir, "sdr.ivf");
  assert!(ffmpeg(&[
    "-i",
    &encodes[0],
    "-c",
    "copy",
    "-bsf:v",
    "av1_metadata=color_primaries=1:transfer_characteristics=1:matrix_coefficients=1",
    &sdr
  ]));
  let retagged = path(&dir, "retagged.ivf");
  let (ok, result) = events(&["apply", &sdr, "-m", &metadata, "-o", &retagged, "--fix-color"]);
  assert!(!ok);
  assert_eq!(event(&result, "error")["code"], "not_hdr10");
  assert!(event(&result, "error")["message"].as_str().unwrap().contains("--force-fix-color"));
  let (ok, result) = events(&["apply", &sdr, "-m", &metadata, "-o", &retagged, "--fix-color", "--force-fix-color"]);
  assert!(ok, "{result:?}");
  let static_out = path(&dir, "static.ivf");
  let (ok, result) = events(&["static", &sdr, "-o", &static_out, "--fix-color"]);
  assert!(!ok);
  assert_eq!(event(&result, "error")["code"], "not_hdr10");

  let (ok, result) = events(&["apply", &encodes[0], "-m", &metadata, "-o", &output]);
  assert!(!ok);
  assert_eq!(event(&result, "error")["code"], "output_exists");

  // HLG stays HLG, even with --fix-color.
  let hlg = path(&dir, "hlg.ivf");
  assert!(ffmpeg(&[
    "-i",
    &encodes[0],
    "-c",
    "copy",
    "-bsf:v",
    "av1_metadata=transfer_characteristics=18",
    &hlg
  ]));
  let refused = path(&dir, "refused.ivf");
  let (ok, result) = events(&["apply", &hlg, "-m", &metadata, "-o", &refused, "--fix-color"]);
  assert!(!ok);
  assert_eq!(event(&result, "error")["code"], "not_hdr10");
  assert!(!Path::new(&refused).exists());
}

#[test]
fn check_reports_broken_metadata() {
  let dir = workspace("check");
  let frame = |percentages: &str, value: u32| {
    format!(
      r#"{{"LuminanceParameters":{{"AverageRGB":10,"LuminanceDistributions":{{"DistributionIndex":{percentages},"DistributionValues":[0,0,255,1,2,3,4,5,{value}]}},"MaxScl":[100,100,100]}},"NumberOfWindows":1,"TargetedSystemDisplayMaximumLuminance":0}}"#
    )
  };
  let good = frame("[1,5,10,25,50,75,90,95,99]", 99);
  let bad = frame("[1,5,10,25,50,75,90,95,98]", 100_001);
  let file = path(&dir, "broken.json");
  std::fs::write(&file, format!(r#"{{"JSONInfo":{{}},"SceneInfo":[{good},{bad}]}}"#)).unwrap();
  let (ok, result) = events(&["check", &file]);
  assert!(!ok);
  let codes: Vec<&str> = result
    .iter()
    .filter(|event| event["event"] == "finding")
    .map(|event| event["code"].as_str().unwrap())
    .collect();
  assert_eq!(codes, vec!["value_range", "percentages"]);
  assert_eq!(event(&result, "error")["code"], "check_failed");
  assert!(result.iter().all(|event| event["frame"] != 0), "the first frame is fine");

  std::fs::write(&file, format!(r#"{{"JSONInfo":{{}},"SceneInfo":[{good}]}}"#)).unwrap();
  assert!(retone(&["check", &file]).status.success());
}

#[test]
fn static_metadata_is_written_and_removed() {
  let dir = workspace("static");
  let Some((_, encodes)) = sample(&dir) else { return skipped() };
  let output = path(&dir, "hdr10.mkv");
  let (ok, result) = events(&[
    "static",
    &encodes[1],
    "-o",
    &output,
    "--mastering-display",
    "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(40000000,50)",
    "--max-cll",
    "1200",
    "--max-fall",
    "300",
  ]);
  assert!(ok, "{result:?}");
  let (_, info) = events(&["info", &output]);
  let info = event(&info, "summary");
  assert_eq!(info["content_light"], serde_json::json!({"max_cll": 1200, "max_fall": 300}));
  assert_eq!(info["mastering_display"]["max_nits"], 4000.0);
  assert_eq!(info["mastering_display"]["min_nits"], 0.005);
  assert_eq!(info["formats"], serde_json::json!(["HDR10"]));

  let bare = path(&dir, "bare.mkv");
  let (ok, result) = events(&["remove", &output, "-o", &bare, "--static", "--keep-hdr10plus"]);
  assert!(ok, "{result:?}");
  assert_eq!(event(&result, "summary")["static"], 2);
  let (_, info) = events(&["info", &bare]);
  assert_eq!(event(&info, "summary")["content_light"], Value::Null);
  assert_eq!(picture(&bare), picture(&encodes[1]));
}

#[test]
fn stats_lists_scenes() {
  let dir = workspace("stats");
  let Some((source, _)) = sample(&dir) else { return skipped() };
  let metadata = path(&dir, "meta.json");
  assert!(retone(&["measure", &source, "-o", &metadata, "--per-frame"]).status.success());
  let (ok, result) = events(&["stats", &metadata]);
  assert!(ok);
  assert_eq!(
    result.iter().filter(|event| event["event"] == "scene").count(),
    event(&result, "summary")["scenes"].as_u64().unwrap() as usize
  );
  let (ok, result) = events(&["stats", &metadata, "--frames"]);
  assert!(ok);
  assert_eq!(result.iter().filter(|event| event["event"] == "frame").count(), 48);
}

#[test]
fn damaged_videos_give_errors_not_crashes() {
  let dir = workspace("damaged");
  let Some((source, encodes)) = sample(&dir) else { return skipped() };
  let metadata = path(&dir, "meta.json");
  let tagged = path(&dir, "tagged.ivf");
  assert!(retone(&["measure", &source, "-o", &metadata]).status.success());
  assert!(retone(&["apply", &encodes[0], "-m", &metadata, "-o", &tagged]).status.success());
  let data = std::fs::read(&tagged).unwrap();
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let mut next = |limit: usize| {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    (state % limit as u64) as usize
  };
  let damaged = path(&dir, "damaged.ivf");
  for round in 0..60 {
    let mut copy = data.clone();
    for _ in 0..1 + next(8) {
      let at = 32 + next(copy.len() - 32);
      copy[at] ^= 1 << next(8);
    }
    std::fs::write(&damaged, &copy).unwrap();
    for command in ["check", "export", "stats", "info"] {
      let mut args = vec![command, damaged.as_str()];
      if command == "export" {
        args.extend(["-o", "-"]);
      }
      let output = retone(&args);
      let stderr = String::from_utf8_lossy(&output.stderr);
      assert!(!stderr.contains("panicked"), "round {round} {command}: {stderr}");
    }
  }
}
