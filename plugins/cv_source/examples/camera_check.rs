//! Reads a few frames from a camera and saves the last one, to check capture
//! on a machine: `cargo run -p flwrs-cv-source --example camera_check --
//! <auto|native|nokhwa> [device] [out.png]`.

use flwrs_cv_source::ingest::camera;
use flwrs_cv_source::ingest::config::{CameraBackend, CameraConfig};
use std::process::ExitCode;
use std::time::Instant;

const FRAMES: u32 = 60;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let backend = match args.next().as_deref() {
        None | Some("auto") => CameraBackend::Auto,
        Some("native") => CameraBackend::Native,
        Some("nokhwa") => CameraBackend::Nokhwa,
        Some(other) => {
            eprintln!("unknown backend {other:?}: auto, native or nokhwa");
            return ExitCode::from(2);
        }
    };
    let device = args.next().filter(|d| !d.is_empty());
    let out = args.next().unwrap_or_else(|| "camera_check.png".into());
    let config = CameraConfig {
        device,
        backend,
        ..CameraConfig::default()
    };
    let workdir = std::env::temp_dir();
    let mut source = match camera::open(&config, &workdir) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("can't open the camera: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("reading {}", source.describe());
    let start = Instant::now();
    let mut last = None;
    for _ in 0..FRAMES {
        match source.next_frame() {
            Ok(Some(captured)) => last = Some(captured.frame),
            Ok(None) => break,
            Err(e) => {
                eprintln!("read failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(frame) = last else {
        eprintln!("no frames");
        return ExitCode::FAILURE;
    };
    println!(
        "{FRAMES} frames of {}x{} at {:.1} fps",
        frame.width(),
        frame.height(),
        f64::from(FRAMES) / start.elapsed().as_secs_f64()
    );
    let saved = image::save_buffer(
        &out,
        frame.rgb(),
        frame.width(),
        frame.height(),
        image::ColorType::Rgb8,
    );
    match saved {
        Ok(()) => {
            println!("saved {out}: check that its colours look right");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("can't save {out}: {e}");
            ExitCode::FAILURE
        }
    }
}
