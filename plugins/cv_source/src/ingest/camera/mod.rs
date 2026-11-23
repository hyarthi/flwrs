//! Cameras of any kind the OS exposes (built-in, USB, CSI, virtual), through
//! its capture API: AVFoundation on macOS (`nokhwa` as a fallback), V4L2 on
//! Linux, Media Foundation on Windows.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // Objective-C APIs; see AGENTS.md
mod macos;
#[cfg(all(target_os = "macos", feature = "camera-nokhwa"))]
mod nokhwa_backend;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(windows)]
#[allow(unsafe_code)] // COM APIs; see AGENTS.md
mod windows;

use super::config::{CameraBackend, CameraConfig};
use super::{Captured, FrameSource, IngestError};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// A read that gets no frame for this long has stalled.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code, reason = "no native backend on this OS")
)]
pub(crate) const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Corrupt frames in a row before a camera counts as failed.
#[cfg_attr(
    not(any(target_os = "linux", windows)),
    allow(dead_code, reason = "AVFoundation's reads are bounded by time")
)]
pub(crate) const MAX_BAD_FRAMES: u32 = 30;

/// A camera as listed by the OS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listed {
    pub id: String,
    pub name: String,
}

/// Picks a camera: none given = the first; else an index, an exact id, or
/// a unique part of the name.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code, reason = "no native backend on this OS")
)]
pub(crate) fn pick(wanted: Option<&str>, cameras: &[Listed]) -> Result<usize, IngestError> {
    let names = || {
        cameras
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{i}: {} ({})", c.name, c.id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if cameras.is_empty() {
        return Err(IngestError::retryable("no camera found"));
    }
    let Some(wanted) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok(0);
    };
    if let Some(i) = cameras.iter().position(|c| c.id == wanted) {
        return Ok(i);
    }
    if let Ok(i) = wanted.parse::<usize>() {
        return if i < cameras.len() {
            Ok(i)
        } else {
            Err(IngestError::retryable(format!(
                "no camera {i}; found {}",
                names()
            )))
        };
    }
    let lower = wanted.to_lowercase();
    let hits: Vec<usize> = (0..cameras.len())
        .filter(|&i| cameras[i].name.to_lowercase().contains(&lower))
        .collect();
    match hits[..] {
        [i] => Ok(i),
        [] => Err(IngestError::retryable(format!(
            "no camera matches {wanted:?}; found {}",
            names()
        ))),
        _ => Err(IngestError::fatal(format!(
            "several cameras match {wanted:?}: {}",
            names()
        ))),
    }
}

/// The offered mode closest to the wanted size (smallest area difference),
/// then able to reach the wanted rate, then the fastest.
#[cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(dead_code, reason = "V4L2 negotiates in the driver")
)]
pub(crate) fn closest_mode(
    modes: &[(u32, u32, f64)],
    size: Option<(u32, u32)>,
    fps: Option<u32>,
) -> Option<usize> {
    (0..modes.len()).min_by_key(|&i| {
        let (w, h, max) = modes[i];
        let area = u64::from(w) * u64::from(h);
        let off = size.map_or(0, |(a, b)| area.abs_diff(u64::from(a) * u64::from(b)));
        let slow = fps.is_some_and(|f| max + 0.01 < f64::from(f));
        // Without a wanted size: a common default, not the sensor's largest.
        let default = if size.is_none() {
            area.abs_diff(1280 * 720)
        } else {
            0
        };
        (off, default, slow, std::cmp::Reverse((max * 1000.0) as u64))
    })
}

/// How long a noted crash keeps `auto` on the fallback.
#[cfg_attr(
    not(all(target_os = "macos", feature = "camera-nokhwa")),
    allow(dead_code, reason = "only macOS has a fallback backend")
)]
const CRASH_NOTE_LIFE: Duration = Duration::from_secs(24 * 3600);

/// The note kept in the node's folder while the native backend starts up.
/// Found at the next start, it means the process died during that start-up
/// (a crash in native code can't be caught from inside).
#[cfg_attr(
    not(all(target_os = "macos", feature = "camera-nokhwa")),
    allow(dead_code, reason = "only macOS has a fallback backend")
)]
fn crash_note(workdir: &Path) -> PathBuf {
    workdir.join(".camera-native-starting")
}

#[cfg_attr(
    not(all(target_os = "macos", feature = "camera-nokhwa")),
    allow(dead_code, reason = "only macOS has a fallback backend")
)]
fn crashed_recently(note: &Path) -> bool {
    std::fs::metadata(note)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < CRASH_NOTE_LIFE)
}

/// A source whose first frame was already read.
#[cfg_attr(
    not(all(target_os = "macos", feature = "camera-nokhwa")),
    allow(dead_code, reason = "only macOS has a fallback backend")
)]
struct Primed {
    first: Option<Captured>,
    inner: Box<dyn FrameSource>,
}

impl FrameSource for Primed {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        match self.first.take() {
            Some(frame) => Ok(Some(frame)),
            None => self.inner.next_frame(),
        }
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

#[cfg_attr(
    not(all(target_os = "macos", feature = "camera-nokhwa")),
    allow(dead_code, reason = "only macOS has a fallback backend")
)]
type Open<'a> = &'a mut dyn FnMut() -> Result<Box<dyn FrameSource>, IngestError>;

/// `auto`: the native backend, or the fallback when the native one fails to
/// open, gives no first frame, or killed the process while starting.
#[cfg_attr(
    not(all(target_os = "macos", feature = "camera-nokhwa")),
    allow(dead_code, reason = "only macOS has a fallback backend")
)]
fn with_fallback(
    workdir: &Path,
    native: Open<'_>,
    fallback: Open<'_>,
) -> Result<Box<dyn FrameSource>, IngestError> {
    let note = crash_note(workdir);
    if crashed_recently(&note) {
        log::warn!(
            "Camera: the native backend died while starting last time; using the fallback (it's tried again after a day, or set input.backend)"
        );
        return fallback();
    }
    if let Err(e) = std::fs::write(&note, b"") {
        log::debug!("Camera: can't keep the start-up note: {e}");
    }
    let started = native().and_then(|mut source| match source.next_frame()? {
        Some(first) => Ok(Box::new(Primed {
            first: Some(first),
            inner: source,
        }) as Box<dyn FrameSource>),
        None => Err(IngestError::retryable("the camera gave no frame")),
    });
    let _ = std::fs::remove_file(&note);
    match started {
        Ok(source) => Ok(source),
        Err(native_error) => {
            log::warn!("Camera: native backend failed ({native_error}); trying the fallback");
            fallback().map_err(|e| IngestError {
                message: format!("{native_error}; the fallback failed too: {e}"),
                retryable: native_error.retryable || e.retryable,
            })
        }
    }
}

/// Opens the configured camera.
pub fn open(
    config: &CameraConfig,
    #[cfg_attr(
        not(all(target_os = "macos", feature = "camera-nokhwa")),
        allow(unused_variables, reason = "only the fallback keeps a note")
    )]
    workdir: &Path,
) -> Result<Box<dyn FrameSource>, IngestError> {
    #[cfg(target_os = "macos")]
    {
        #[cfg_attr(not(feature = "camera-nokhwa"), allow(unused_mut))]
        let mut native = || macos::open(config).map(|s| Box::new(s) as Box<dyn FrameSource>);
        #[cfg(feature = "camera-nokhwa")]
        let mut fallback =
            || nokhwa_backend::open(config).map(|s| Box::new(s) as Box<dyn FrameSource>);
        match config.backend {
            CameraBackend::Native => native(),
            #[cfg(feature = "camera-nokhwa")]
            CameraBackend::Nokhwa => fallback(),
            #[cfg(feature = "camera-nokhwa")]
            CameraBackend::Auto => with_fallback(workdir, &mut native, &mut fallback),
            #[cfg(not(feature = "camera-nokhwa"))]
            CameraBackend::Auto => native(),
            #[cfg(not(feature = "camera-nokhwa"))]
            CameraBackend::Nokhwa => Err(IngestError::fatal("nokhwa isn't in this build")),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match config.backend {
            CameraBackend::Nokhwa => Err(IngestError::fatal("nokhwa is macOS only")),
            _ => v4l2::open(config).map(|s| Box::new(s) as Box<dyn FrameSource>),
        }
    }
    #[cfg(windows)]
    {
        match config.backend {
            CameraBackend::Nokhwa => Err(IngestError::fatal("nokhwa is macOS only")),
            _ => windows::open(config).map(|s| Box::new(s) as Box<dyn FrameSource>),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (config, CameraBackend::Auto);
        Err(IngestError::fatal(
            "this build has no camera support on this OS; use a file input",
        ))
    }
}

/// What an OS callback delivered.
#[cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(dead_code, reason = "backends with blocking reads")
)]
pub(crate) enum Delivery {
    Frame(Captured),
    /// A frame that couldn't be converted.
    Bad(String),
    /// Nothing this time (a stream tick): read again.
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        allow(dead_code, reason = "AVFoundation has no stream ticks")
    )]
    Again,
    /// The stream broke.
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        allow(dead_code, reason = "AVFoundation reports a stop by going quiet")
    )]
    Failed(String),
}

/// A one-item slot between an OS callback thread and the reader, so a read
/// can give up when the camera stops delivering.
#[derive(Default)]
#[cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(dead_code, reason = "backends with blocking reads")
)]
pub(crate) struct DeliverySlot {
    latest: Mutex<Option<Delivery>>,
    ready: Condvar,
}

#[cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(dead_code, reason = "backends with blocking reads")
)]
impl DeliverySlot {
    /// Replaces what's unread.
    pub fn put(&self, delivery: Delivery) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(delivery);
        self.ready.notify_all();
    }

    /// The next delivery, or `None` once `until` passes.
    pub fn wait(&self, until: Instant) -> Option<Delivery> {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(delivery) = latest.take() {
                return Some(delivery);
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            latest = self
                .ready
                .wait_timeout(latest, until - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use std::cell::Cell;

    fn cams() -> Vec<Listed> {
        [
            ("id-a", "FaceTime HD Camera"),
            ("id-b", "USB Camera"),
            ("id-c", "USB Capture"),
        ]
        .map(|(id, name)| Listed {
            id: id.into(),
            name: name.into(),
        })
        .to_vec()
    }

    #[test]
    fn cameras_are_picked_by_index_id_or_name() {
        let c = cams();
        assert_eq!(pick(None, &c).unwrap(), 0);
        assert_eq!(pick(Some("2"), &c).unwrap(), 2);
        assert_eq!(pick(Some("id-b"), &c).unwrap(), 1);
        assert_eq!(pick(Some("facetime"), &c).unwrap(), 0);
        assert!(!pick(Some("usb"), &c).unwrap_err().retryable, "ambiguous");
        assert!(
            pick(Some("9"), &c).unwrap_err().retryable,
            "may be plugged in"
        );
        assert!(pick(None, &[]).unwrap_err().retryable);
    }

    #[test]
    fn the_closest_mode_wins() {
        let m = [
            (640, 480, 30.0),
            (1280, 720, 15.0),
            (1280, 720, 60.0),
            (1920, 1080, 30.0),
        ];
        assert_eq!(closest_mode(&m, Some((640, 480)), None), Some(0));
        assert_eq!(closest_mode(&m, Some((1280, 720)), Some(30)), Some(2));
        assert_eq!(closest_mode(&m, None, None), Some(2));
        assert_eq!(closest_mode(&[], None, None), None);
    }

    #[test]
    fn a_delivery_slot_waits_and_gives_up() {
        let slot = std::sync::Arc::new(DeliverySlot::default());
        let until = Instant::now() + Duration::from_millis(20);
        assert!(slot.wait(until).is_none(), "nothing arrives: gives up");
        let inner = slot.clone();
        std::thread::spawn(move || inner.put(Delivery::Failed("gone".into())));
        let got = slot.wait(Instant::now() + Duration::from_secs(10));
        assert!(matches!(got, Some(Delivery::Failed(m)) if m == "gone"));
        slot.put(Delivery::Again);
        slot.put(Delivery::Bad("newest".into()));
        assert!(matches!(slot.wait(Instant::now()), Some(Delivery::Bad(_))));
        assert!(slot.wait(Instant::now()).is_none());
    }

    struct One;
    impl FrameSource for One {
        fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
            Ok(Some(Captured {
                frame: Frame::filled(1, 1, [1; 3]).unwrap(),
                capture_time: SystemTime::now(),
            }))
        }
        fn describe(&self) -> String {
            "one".into()
        }
    }

    fn ok() -> Result<Box<dyn FrameSource>, IngestError> {
        Ok(Box::new(One))
    }

    #[test]
    fn a_working_native_backend_leaves_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let used = Cell::new(false);
        with_fallback(dir.path(), &mut ok, &mut || {
            used.set(true);
            ok()
        })
        .unwrap();
        assert!(!used.get());
        assert!(!crash_note(dir.path()).exists());
    }

    #[test]
    fn a_failing_native_backend_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let used = Cell::new(false);
        let mut bad = || Err(IngestError::retryable("busy"));
        with_fallback(dir.path(), &mut bad, &mut || {
            used.set(true);
            ok()
        })
        .unwrap();
        assert!(used.get());
        let mut bad2 = || Err(IngestError::fatal("also"));
        let e = with_fallback(dir.path(), &mut bad, &mut bad2)
            .err()
            .unwrap();
        assert!(
            e.message.contains("busy") && e.message.contains("also"),
            "{e}"
        );
    }

    #[test]
    fn a_crash_during_start_up_goes_to_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(crash_note(dir.path()), b"").unwrap();
        let tried = Cell::new(false);
        with_fallback(
            dir.path(),
            &mut || {
                tried.set(true);
                ok()
            },
            &mut ok,
        )
        .unwrap();
        assert!(!tried.get(), "the native backend isn't tried again");
        assert!(
            crash_note(dir.path()).exists(),
            "the note stays for its life"
        );
    }
}
