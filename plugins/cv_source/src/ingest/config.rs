//! The `input` setting: where frames come from.

use serde::Deserialize;

/// Where a node's frames come from.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputConfig {
    /// A camera the OS knows: built-in, USB, CSI, virtual.
    Camera(CameraConfig),
    /// A folder of JPEG / PNG images, in name order.
    Images(FileInput),
    /// A file of concatenated JPEG frames.
    Mjpeg(FileInput),
}

/// A camera and how to read it. What's left out is the camera's choice.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// An index, a device id or path, or part of the camera's name.
    pub device: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    #[serde(default)]
    pub format: CameraFormat,
    #[serde(default)]
    pub backend: CameraBackend,
}

/// The pixel format asked of the camera.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CameraFormat {
    #[default]
    Auto,
    Mjpeg,
    Yuyv,
    Uyvy,
    Nv12,
}

/// Which capture code reads the camera.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CameraBackend {
    /// The OS's own API; on macOS, `nokhwa` if that fails.
    #[default]
    Auto,
    /// The OS's own API, with no fallback.
    Native,
    /// `nokhwa` (macOS builds with the `camera-nokhwa` feature).
    Nokhwa,
}

/// A file input.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileInput {
    /// A node file's name, or a path.
    pub path: String,
    /// Frames per second. Left out: every frame, as fast as they're taken
    /// (none dropped: the deterministic mode tests use).
    pub fps: Option<f32>,
    /// Start over at the end.
    #[serde(default, rename = "loop")]
    pub looped: bool,
}

impl InputConfig {
    /// A file read frame by frame, so a run repeats exactly.
    pub fn is_deterministic(&self) -> bool {
        matches!(self, Self::Images(f) | Self::Mjpeg(f) if f.fps.is_none())
    }

    /// Every problem with the settings.
    pub fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        match self {
            Self::Camera(c) => {
                for (name, v, max) in [
                    ("width", c.width, crate::frame::MAX_FRAME_SIDE),
                    ("height", c.height, crate::frame::MAX_FRAME_SIDE),
                    ("fps", c.fps, 1000),
                ] {
                    if v.is_some_and(|v| v == 0 || v > max) {
                        out.push(format!("input.{name}: must be 1 to {max}"));
                    }
                }
                if c.width.is_some() != c.height.is_some() {
                    out.push("input: width and height go together".into());
                }
                if c.backend == CameraBackend::Nokhwa
                    && !cfg!(all(target_os = "macos", feature = "camera-nokhwa"))
                {
                    out.push(
                        "input.backend: nokhwa isn't in this build (macOS, feature camera-nokhwa)"
                            .into(),
                    );
                }
            }
            Self::Images(f) | Self::Mjpeg(f) => {
                if f.path.is_empty() {
                    out.push("input.path: empty".into());
                }
                if f.fps.is_some_and(|v| !(0.1..=1000.0).contains(&v)) {
                    out.push("input.fps: must be 0.1 to 1000".into());
                }
            }
        }
        out
    }
}
