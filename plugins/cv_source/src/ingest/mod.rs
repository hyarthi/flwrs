//! Frames in: a [`FrameSource`] (a camera, a folder, an MJPEG file) read on
//! its own thread into a latest-frame-wins buffer, so a slow graph always
//! gets the newest frame and a camera is never held up.

pub mod camera;
pub mod config;
pub mod convert;
pub mod files;

use crate::frame::Frame;
use config::InputConfig;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// A frame as read.
#[derive(Clone, Debug)]
pub struct Captured {
    pub frame: Frame,
    /// When it reached this process.
    pub capture_time: SystemTime,
}

/// Why reading stopped. A retryable error (an unplugged camera) reopens the
/// source with a backoff.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct IngestError {
    pub message: String,
    pub retryable: bool,
}

impl IngestError {
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
}

/// Something frames are read from, on the thread that opened it (camera
/// handles often can't move between threads). Reads block, for a bounded time.
pub trait FrameSource {
    /// The next frame; `None` at the end.
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError>;

    /// When the next frame is due, for sources that pace themselves.
    fn due(&self) -> Option<Instant> {
        None
    }

    /// Wait for the consumer instead of replacing an unread frame.
    fn lossless(&self) -> bool {
        false
    }

    /// The time between frames, for a source with its own timeline (a
    /// file): the stream's clock then counts frames, so runs repeat exactly.
    /// `None`: the clock is real time.
    fn frame_interval(&self) -> Option<Duration> {
        None
    }

    /// For logs.
    fn describe(&self) -> String;
}

/// Opens (and reopens) a source.
pub type Opener = Box<dyn FnMut() -> Result<Box<dyn FrameSource>, IngestError> + Send>;

/// An opener for the settings. `files`: the node's files by name; `workdir`:
/// the node's folder (the camera fallback keeps its note there).
pub fn opener(input: &InputConfig, files: &BTreeMap<String, PathBuf>, workdir: &Path) -> Opener {
    let resolve = |path: &str| {
        files
            .get(path)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(path))
    };
    match input.clone() {
        InputConfig::Images(f) => {
            let path = resolve(&f.path);
            Box::new(move || Ok(Box::new(files::ImageFolder::open(&f, &path)?) as _))
        }
        InputConfig::Mjpeg(f) => {
            let path = resolve(&f.path);
            Box::new(move || Ok(Box::new(files::MjpegFile::open(&f, &path)?) as _))
        }
        InputConfig::Camera(c) => {
            let workdir = workdir.to_path_buf();
            Box::new(move || camera::open(&c, &workdir))
        }
    }
}

/// Counters since the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestStats {
    /// Frames read from the source.
    pub captured: u64,
    /// Frames replaced before anyone took them.
    pub dropped: u64,
    /// Frames handed to the consumer.
    pub taken: u64,
    /// Times the source was reopened.
    pub reopened: u64,
}

/// A frame for the consumer.
#[derive(Clone, Debug)]
pub struct Item {
    pub captured: Captured,
    /// Counts frames read, so a gap shows the frames skipped.
    pub seq: u64,
    /// On the stream's clock: what rules measure durations by. Never goes
    /// back, also across a reopened camera.
    pub time: Duration,
}

/// What [`Ingest::next`] found.
#[derive(Debug)]
pub enum Next {
    Frame(Item),
    /// Nothing within the wait.
    Timeout,
    /// The source ended.
    Ended,
    /// The source failed for good.
    Failed(String),
}

#[derive(Default)]
struct State {
    slot: Option<Item>,
    end: Option<Option<String>>,
    stats: IngestStats,
}

/// A function given every frame as it's read.
pub type FrameTap = Box<dyn Fn(&Frame) + Send>;

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Sees every frame read, also those the graph won't get to.
    tap: Mutex<Option<FrameTap>>,
    changed: Condvar,
    stop: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Waits until `deadline` or a stop; false when stopping.
    fn sleep_until(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        while !self.stopping() {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        false
    }

    /// Hands over a frame; false when stopping.
    fn put(&self, captured: Captured, clock: Clock, lossless: bool) -> bool {
        if let Some(tap) = self.tap.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            tap(&captured.frame);
        }
        let mut state = self.lock();
        while lossless && state.slot.is_some() && !self.stopping() {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if self.stopping() {
            return false;
        }
        let seq = state.stats.captured;
        state.stats.captured += 1;
        let time = match clock.interval {
            Some(every) => every.mul_f64(seq as f64),
            None => clock.start.elapsed(),
        };
        if state
            .slot
            .replace(Item {
                captured,
                seq,
                time,
            })
            .is_some()
        {
            state.stats.dropped += 1;
        }
        drop(state);
        self.changed.notify_all();
        true
    }

    fn finish(&self, end: Option<String>) {
        self.lock().end = Some(end);
        self.changed.notify_all();
    }
}

const REOPEN_FIRST: Duration = Duration::from_secs(1);
const REOPEN_MAX: Duration = Duration::from_secs(30);

/// How a frame gets its time on the stream's clock.
#[derive(Clone, Copy)]
struct Clock {
    start: Instant,
    interval: Option<Duration>,
}

fn pump(shared: &Shared, mut open: Opener) {
    let start = Instant::now();
    let mut backoff = REOPEN_FIRST;
    let mut opened_before = false;
    'open: while !shared.stopping() {
        let mut source = match open() {
            Ok(source) => source,
            Err(e) if e.retryable => {
                log::warn!("Input: {e}; retrying in {backoff:?}");
                if !shared.sleep_until(Instant::now() + backoff) {
                    break;
                }
                backoff = (backoff * 2).min(REOPEN_MAX);
                continue;
            }
            Err(e) => return shared.finish(Some(e.message)),
        };
        if opened_before {
            shared.lock().stats.reopened += 1;
        }
        opened_before = true;
        log::info!("Input: reading {}", source.describe());
        loop {
            if let Some(due) = source.due()
                && !shared.sleep_until(due)
            {
                break 'open;
            }
            match source.next_frame() {
                Ok(Some(captured)) => {
                    backoff = REOPEN_FIRST;
                    let clock = Clock {
                        start,
                        interval: source.frame_interval(),
                    };
                    if !shared.put(captured, clock, source.lossless()) {
                        break 'open;
                    }
                }
                Ok(None) => return shared.finish(None),
                Err(e) if e.retryable => {
                    log::warn!("Input: {e}; reopening in {backoff:?}");
                    drop(source);
                    if !shared.sleep_until(Instant::now() + backoff) {
                        break 'open;
                    }
                    backoff = (backoff * 2).min(REOPEN_MAX);
                    continue 'open;
                }
                Err(e) => return shared.finish(Some(e.message)),
            }
        }
    }
    shared.finish(None);
}

/// A source being read on its own thread. Dropping it stops the thread.
pub struct Ingest {
    shared: Arc<Shared>,
}

impl Ingest {
    pub fn start(open: Opener) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let inner = shared.clone();
        std::thread::Builder::new()
            .name("cv-ingest".into())
            .spawn(move || pump(&inner, open))?;
        Ok(Self { shared })
    }

    /// The newest frame not taken yet, waiting up to `wait`.
    pub fn next(&self, wait: Duration) -> Next {
        let deadline = Instant::now() + wait;
        let mut state = self.shared.lock();
        loop {
            if let Some(item) = state.slot.take() {
                state.stats.taken += 1;
                drop(state);
                self.shared.changed.notify_all();
                return Next::Frame(item);
            }
            if let Some(end) = &state.end {
                return end.clone().map_or(Next::Ended, Next::Failed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Next::Timeout;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Sets (or clears) what sees every frame read.
    pub fn set_tap(&self, tap: Option<FrameTap>) {
        *self.shared.tap.lock().unwrap_or_else(|e| e.into_inner()) = tap;
    }

    pub fn stats(&self) -> IngestStats {
        self.shared.lock().stats
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // Under the lock, so a waiter between its check and its wait sees it.
        let _guard = self.shared.lock();
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(10);

    fn frame(v: u8) -> Captured {
        Captured {
            frame: Frame::filled(2, 2, [v; 3]).unwrap(),
            capture_time: SystemTime::now(),
        }
    }

    /// Gives what the test sends, one read each.
    struct Script {
        steps: mpsc::Receiver<Result<Option<Captured>, IngestError>>,
        lossless: bool,
    }

    impl FrameSource for Script {
        fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
            self.steps.recv().unwrap_or(Ok(None))
        }
        fn lossless(&self) -> bool {
            self.lossless
        }
        fn describe(&self) -> String {
            "script".into()
        }
    }

    fn scripted(lossless: bool) -> (mpsc::Sender<Result<Option<Captured>, IngestError>>, Ingest) {
        let (tx, rx) = mpsc::channel();
        let mut rx = Some(rx);
        let ingest = Ingest::start(Box::new(move || {
            Ok(Box::new(Script {
                steps: rx.take().expect("opened once"),
                lossless,
            }) as _)
        }))
        .unwrap();
        (tx, ingest)
    }

    fn until(ingest: &Ingest, what: impl Fn(IngestStats) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !what(ingest.stats()) {
            assert!(Instant::now() < deadline, "{:?}", ingest.stats());
            std::thread::yield_now();
        }
    }

    #[test]
    fn the_latest_frame_wins_and_drops_are_counted() {
        let (tx, ingest) = scripted(false);
        for v in 0..5 {
            tx.send(Ok(Some(frame(v)))).unwrap();
        }
        until(&ingest, |s| s.captured == 5);
        let Next::Frame(item) = ingest.next(WAIT) else {
            panic!("no frame")
        };
        assert_eq!((item.seq, item.captured.frame.rgb()[0]), (4, 4));
        assert_eq!(
            ingest.stats(),
            IngestStats {
                captured: 5,
                dropped: 4,
                taken: 1,
                reopened: 0
            }
        );
        assert!(matches!(ingest.next(Duration::ZERO), Next::Timeout));
        tx.send(Ok(None)).unwrap();
        assert!(matches!(ingest.next(WAIT), Next::Ended));
    }

    #[test]
    fn a_lossless_source_waits_for_the_consumer() {
        let (tx, ingest) = scripted(true);
        for v in 0..4 {
            tx.send(Ok(Some(frame(v)))).unwrap();
        }
        tx.send(Ok(None)).unwrap();
        for v in 0..4 {
            let Next::Frame(item) = ingest.next(WAIT) else {
                panic!("no frame")
            };
            assert_eq!(item.captured.frame.rgb()[0], v);
        }
        assert!(matches!(ingest.next(WAIT), Next::Ended));
        assert_eq!(ingest.stats().dropped, 0);
    }

    #[test]
    fn a_fatal_error_is_reported_after_the_last_frame() {
        let (tx, ingest) = scripted(false);
        tx.send(Ok(Some(frame(7)))).unwrap();
        tx.send(Err(IngestError::fatal("broken"))).unwrap();
        assert!(matches!(ingest.next(WAIT), Next::Frame(_)));
        assert!(matches!(ingest.next(WAIT), Next::Failed(m) if m == "broken"));
    }

    #[test]
    fn a_retryable_failure_reopens_the_source() {
        struct Once(VecDeque<Result<Option<Captured>, IngestError>>);
        impl FrameSource for Once {
            fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
                self.0.pop_front().unwrap_or(Ok(None))
            }
            fn describe(&self) -> String {
                "once".into()
            }
        }
        let mut opens = 0;
        let ingest = Ingest::start(Box::new(move || {
            opens += 1;
            let steps = if opens == 1 {
                vec![Err(IngestError::retryable("unplugged"))]
            } else {
                vec![Ok(Some(frame(9)))]
            };
            Ok(Box::new(Once(steps.into())) as _)
        }))
        .unwrap();
        let Next::Frame(item) = ingest.next(WAIT) else {
            panic!("no frame")
        };
        assert_eq!(item.captured.frame.rgb()[0], 9);
        assert_eq!(ingest.stats().reopened, 1);
    }

    /// A node's file by name, through the opener, every frame in order.
    #[test]
    fn an_opener_reads_a_node_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stored_abc.mjpeg");
        let mut data = vec![];
        for v in [30u8, 220] {
            let image = image::RgbImage::from_pixel(8, 8, image::Rgb([v; 3]));
            let mut out = std::io::Cursor::new(vec![]);
            image.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
            data.extend(out.into_inner());
        }
        std::fs::write(&path, data).unwrap();
        let files = BTreeMap::from([("clip.mjpeg".to_string(), path)]);
        let input = InputConfig::Mjpeg(config::FileInput {
            path: "clip.mjpeg".into(),
            fps: None,
            looped: false,
        });
        let ingest = Ingest::start(opener(&input, &files, dir.path())).unwrap();
        for v in [30u8, 220] {
            let Next::Frame(item) = ingest.next(WAIT) else {
                panic!("no frame")
            };
            assert!(item.captured.frame.rgb()[0].abs_diff(v) < 8);
        }
        assert!(matches!(ingest.next(WAIT), Next::Ended));
    }

    #[test]
    fn dropping_stops_a_blocked_lossless_reader() {
        let (tx, ingest) = scripted(true);
        tx.send(Ok(Some(frame(1)))).unwrap();
        tx.send(Ok(Some(frame(2)))).unwrap();
        until(&ingest, |s| s.captured == 1);
        let shared = ingest.shared.clone();
        drop(ingest);
        let deadline = Instant::now() + WAIT;
        while shared.lock().end.is_none() {
            assert!(Instant::now() < deadline, "the reader didn't stop");
            std::thread::yield_now();
        }
    }
}
