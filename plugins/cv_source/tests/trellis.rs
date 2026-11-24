//! The vision source checked with trellis, as a dev-dependency: the protocol
//! audit against the binary cargo built for this test, idle and while reading
//! frames, and the events it sends from a file of frames.

use flwrs_trellis::{Audit, Outcome, Target, Tester};
use std::path::Path;

#[tokio::test]
async fn follows_the_plugin_protocol() {
    let target = Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-cv-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-cv-source-config"))
        .with_launch_args(serde_json::json!({
            "graph": { "nodes": { "camera": { "type": "source" } } }
        }))
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results
            .iter()
            .all(|r| matches!(r.outcome, Outcome::Pass | Outcome::Skip(_))),
        "{}",
        flwrs_trellis::report(&results)
    );
}

/// Reading frames in a loop doesn't get in the way of the protocol
/// (shutdown, reconnects).
#[tokio::test]
async fn follows_the_protocol_while_reading_frames() {
    let dir = tempfile::tempdir().unwrap();
    image::RgbImage::from_pixel(16, 16, image::Rgb([40, 90, 200]))
        .save(dir.path().join("a.png"))
        .unwrap();
    let target = Target::from_package(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-cv-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-cv-source-config"))
        .with_launch_args(serde_json::json!({
            "graph": { "nodes": { "camera": { "type": "source" } } },
            "input": { "type": "images", "path": dir.path(), "fps": 30, "loop": true }
        }))
        .unwrap();
    let results = Audit::new(target).run().await;
    assert!(
        results
            .iter()
            .all(|r| matches!(r.outcome, Outcome::Pass | Outcome::Skip(_))),
        "{}",
        flwrs_trellis::report(&results)
    );
}

/// A short clip of plain frames, as the file input reads it.
fn clip(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("clip.mjpeg");
    let mut data = vec![];
    for _ in 0..40 {
        let frame = image::RgbImage::from_pixel(64, 32, image::Rgb([60, 90, 120]));
        let mut out = std::io::Cursor::new(vec![]);
        frame.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        data.extend(out.into_inner());
    }
    std::fs::write(&path, data).unwrap();
    path
}

/// A fixed detector (a person and a dog) through a tracker and rules: the
/// payloads as they reach the hub.
#[tokio::test]
async fn rule_events_and_frames_reach_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = Target::from_package(root)
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-cv-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-cv-source-config"))
        .with_upload("fixed_det.onnx", root.join("tests/trellis/fixed_det.onnx"))
        .unwrap()
        .with_upload("clip.mjpeg", clip(dir.path()))
        .unwrap()
        .with_launch_args(serde_json::json!({
            "graph": {
                "nodes": {
                    "camera": { "type": "source" },
                    "det": {
                        "type": "model", "model": "fixed_det.onnx",
                        "pre": { "width": 64, "height": 32, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw" },
                        "decode": { "type": "detections_e2e", "output": "rows" },
                        "labels": ["person", "dog"]
                    },
                    "people": { "type": "filter", "classes": ["person"] },
                    "tracks": { "type": "tracker" },
                    "arrived": { "type": "rule", "rule": { "type": "track", "on": "started" } },
                    "out": { "type": "emit", "mode": "changes", "min_interval_ms": 0 }
                },
                "links": [
                    { "from": "camera.frame", "to": "det.frame" },
                    { "from": "det.objects", "to": "people.objects" },
                    { "from": "people.objects", "to": "tracks.objects" },
                    { "from": "tracks.objects", "to": "arrived.objects" },
                    { "from": "tracks.objects", "to": "out.objects" }
                ]
            },
            "files": ["fixed_det.onnx", "clip.mjpeg"],
            "input": { "type": "mjpeg", "path": "clip.mjpeg", "fps": 30 }
        }))
        .unwrap();
    let mut tester = Tester::new(target);
    let result = tester
        .run_file(&root.join("tests/trellis/events.test.toml"))
        .await;
    assert_eq!(
        result.outcome,
        Outcome::Pass,
        "{}",
        flwrs_trellis::testing::report(std::slice::from_ref(&result))
    );
}

/// The configurator: what it writes at a save, and its pre-flight answers.
#[tokio::test]
async fn the_configurator_derives_settings_and_checks_the_machine() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = Target::from_package(root)
        .unwrap()
        .with_executable(env!("CARGO_BIN_EXE_flwrs-cv-source"))
        .with_configurator_executable(env!("CARGO_BIN_EXE_flwrs-cv-source-config"));
    let mut tester = Tester::new(target).update(std::env::var_os("FLWRS_UPDATE").is_some());
    for file in [
        "detector.configure.toml",
        "broken.configure.toml",
        "preflight.configure.toml",
        "missing_file.configure.toml",
        "no_camera.configure.toml",
    ] {
        let result = tester
            .run_file(&root.join("tests/trellis").join(file))
            .await;
        assert_eq!(
            result.outcome,
            Outcome::Pass,
            "{file}: {}",
            flwrs_trellis::testing::report(std::slice::from_ref(&result))
        );
    }
}
