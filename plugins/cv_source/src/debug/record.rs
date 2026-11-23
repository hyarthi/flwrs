//! Recording what the camera delivers, as an MJPEG file the debug mode can
//! read back (`--video`). Frames are encoded on a thread of their own and
//! dropped if it falls behind, so recording never slows the graph.

use crate::frame::Frame;
use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const QUALITY: u8 = 90;
/// Frames waiting to be encoded before new ones are dropped.
const QUEUE: usize = 32;

/// A finished recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    pub path: PathBuf,
    pub frames: u64,
    pub dropped: u64,
    pub seconds: f32,
    pub error: Option<String>,
}

impl Recorded {
    /// The frame rate to play it back at.
    pub fn fps(&self) -> f32 {
        if self.seconds > 0.0 && self.frames > 1 {
            (self.frames - 1) as f32 / self.seconds
        } else {
            30.0
        }
    }
}

#[derive(Default)]
struct Times {
    first: Option<Instant>,
    last: Option<Instant>,
    dropped: u64,
}

/// Takes frames from any thread.
#[derive(Clone)]
pub struct Tap {
    queue: SyncSender<Frame>,
    times: Arc<Mutex<Times>>,
}

impl Tap {
    pub fn push(&self, frame: &Frame) {
        let now = Instant::now();
        let mut t = self.times.lock().unwrap_or_else(|e| e.into_inner());
        match self.queue.try_send(frame.clone()) {
            Ok(()) => {
                t.first.get_or_insert(now);
                t.last = Some(now);
            }
            Err(TrySendError::Full(_)) => t.dropped += 1,
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// A recording in progress.
pub struct Recorder {
    tap: Tap,
    path: PathBuf,
    started: Instant,
    worker: JoinHandle<(u64, Option<String>)>,
}

fn encode(file: File, frames: mpsc::Receiver<Frame>) -> (u64, Option<String>) {
    let mut out = BufWriter::new(file);
    let mut written = 0;
    let mut scratch = vec![];
    for frame in frames {
        scratch.clear();
        let done = JpegEncoder::new_with_quality(&mut scratch, QUALITY)
            .encode(
                frame.rgb(),
                frame.width(),
                frame.height(),
                ExtendedColorType::Rgb8,
            )
            .map_err(|e| e.to_string())
            .and_then(|()| out.write_all(&scratch).map_err(|e| e.to_string()));
        if let Err(e) = done {
            // Dropping the receiver makes later pushes no-ops.
            return (written, Some(e));
        }
        written += 1;
    }
    let flushed = out.flush().err().map(|e| e.to_string());
    (written, flushed)
}

impl Recorder {
    /// Starts a new file in `dir` (created if needed).
    pub fn start(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let path = dir.join(format!("cv-{stamp}.mjpeg"));
        let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (queue, frames) = mpsc::sync_channel(QUEUE);
        let worker = std::thread::Builder::new()
            .name("cv-record".into())
            .spawn(move || encode(file, frames))
            .map_err(|e| format!("can't start recording: {e}"))?;
        Ok(Self {
            tap: Tap {
                queue,
                times: Arc::default(),
            },
            path,
            started: Instant::now(),
            worker,
        })
    }

    pub fn tap(&self) -> Tap {
        self.tap.clone()
    }

    /// Seconds so far.
    pub fn elapsed(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// Finishes the file. Every clone of the tap must be gone first.
    pub fn stop(self) -> Recorded {
        let Self {
            tap, path, worker, ..
        } = self;
        let times = tap.times.clone();
        drop(tap);
        let (frames, error) = worker
            .join()
            .unwrap_or_else(|_| (0, Some("the encoder panicked".into())));
        let t = times.lock().unwrap_or_else(|e| e.into_inner());
        let seconds = match (t.first, t.last) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f32(),
            _ => 0.0,
        };
        Recorded {
            path,
            frames,
            dropped: t.dropped,
            seconds,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::FrameSource;
    use crate::ingest::config::FileInput;
    use crate::ingest::files::MjpegFile;

    #[test]
    fn a_recording_plays_back_frame_for_frame() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::start(&dir.path().join("new/folder")).unwrap();
        let tap = r.tap();
        let values = [20u8, 120, 230];
        for v in values {
            // Waits for room: this test wants every frame.
            tap.queue
                .send(Frame::filled(32, 16, [v; 3]).unwrap())
                .unwrap();
        }
        drop(tap);
        let done = r.stop();
        assert_eq!((done.frames, done.error.clone()), (3, None));
        let input = FileInput {
            path: done.path.display().to_string(),
            fps: None,
            looped: false,
        };
        let mut source = MjpegFile::open(&input, &done.path).unwrap();
        for v in values {
            let f = source.next_frame().unwrap().unwrap().frame;
            assert_eq!((f.width(), f.height()), (32, 16));
            assert!(f.rgb()[0].abs_diff(v) < 6, "{} vs {v}", f.rgb()[0]);
        }
        assert!(source.next_frame().unwrap().is_none());
    }

    #[test]
    fn a_full_queue_drops_instead_of_waiting() {
        let (queue, frames) = mpsc::sync_channel(1);
        let tap = Tap {
            queue,
            times: Arc::default(),
        };
        let f = Frame::filled(2, 2, [1; 3]).unwrap();
        tap.push(&f);
        tap.push(&f);
        tap.push(&f);
        assert_eq!(tap.times.lock().unwrap().dropped, 2);
        drop(frames);
        tap.push(&f);
    }

    #[test]
    fn the_playback_rate_comes_from_the_timing() {
        let r = Recorded {
            path: "x".into(),
            frames: 31,
            dropped: 0,
            seconds: 2.0,
            error: None,
        };
        assert_eq!(r.fps(), 15.0);
    }
}
