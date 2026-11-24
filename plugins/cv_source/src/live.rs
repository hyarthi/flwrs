//! The node's live view, on the protocol's live feed: per-node stats and an
//! annotated video preview, each switched on by the `live` setting.
//!
//! Neither costs anything unless someone is watching: with its channel
//! unwatched, the frame loop only asks whether it is. Stats are added up on
//! the frame loop (a few counters per frame). The preview's drawing and JPEG
//! encoding run on a thread of their own, fed through a one-frame slot where
//! a newer frame replaces an older one, so a slow encoder can't delay
//! inference or events. Frames stay in memory.

use crate::debug::compose::{Scene, View, annotate};
use crate::debug::facts::{self, Layouts, RuleShape};
use crate::frame::Frame;
use crate::graph::config::{GraphConfig, NodeConfig};
use crate::graph::exec::Tick;
use crate::ingest::IngestStats;
use flwrs_plugin::plugin::live::{LiveFeedError, LiveFeedPublisher};
use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// The stats channel.
pub const STATS_CHANNEL: &str = "stats";
/// The preview channel.
pub const PREVIEW_CHANNEL: &str = "preview";
const STATS_MEDIA_TYPE: &str = "application/json";
const PREVIEW_MEDIA_TYPE: &str = "image/jpeg";

/// How often stats are sent.
const STATS_EVERY: Duration = Duration::from_secs(1);
/// Latency samples kept per series and interval; a longer interval keeps the
/// newest ones.
const MAX_SAMPLES: usize = 256;
/// The largest preview frame sent, well under the hub's default limit.
const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;
/// JPEG qualities tried in turn until a frame fits.
const QUALITIES: [u8; 4] = [75, 60, 45, 30];

pub const MIN_PREVIEW_FPS: f32 = 0.5;
pub const MAX_PREVIEW_FPS: f32 = 30.0;
pub const MIN_PREVIEW_WIDTH: u32 = 160;
pub const MAX_PREVIEW_WIDTH: u32 = 1920;

/// The node's `live` setting.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LiveConfig {
    /// Per-node stats while someone watches.
    pub stats: bool,
    pub preview: PreviewConfig,
}

/// The annotated video preview.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PreviewConfig {
    pub enabled: bool,
    /// At most this many frames per second.
    pub fps: f32,
    /// Width in pixels; a smaller frame isn't enlarged.
    pub width: u32,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fps: 5.0,
            width: 640,
        }
    }
}

impl LiveConfig {
    /// Whether the node has a live feed at all.
    pub fn any(&self) -> bool {
        self.stats || self.preview.enabled
    }

    pub fn problems(&self) -> Vec<String> {
        let mut problems = vec![];
        let fps = self.preview.fps;
        if !fps.is_finite() || !(MIN_PREVIEW_FPS..=MAX_PREVIEW_FPS).contains(&fps) {
            problems.push(format!(
                "live.preview.fps must be {MIN_PREVIEW_FPS} to {MAX_PREVIEW_FPS}, not {fps}"
            ));
        }
        let width = self.preview.width;
        if !(MIN_PREVIEW_WIDTH..=MAX_PREVIEW_WIDTH).contains(&width) {
            problems.push(format!(
                "live.preview.width must be {MIN_PREVIEW_WIDTH} to {MAX_PREVIEW_WIDTH}, not {width}"
            ));
        }
        problems
    }
}

/// Where the feed goes: the SDK's publisher, or a test's.
pub trait Feed: Clone + Send + Sync + 'static {
    fn announce(&self, channel: &str, media_type: &str) -> Result<(), LiveFeedError>;
    fn is_watched(&self, channel: &str) -> bool;
    fn publish(&self, channel: &str, data: Vec<u8>, capture_time: Option<SystemTime>) -> bool;
}

impl Feed for LiveFeedPublisher {
    fn announce(&self, channel: &str, media_type: &str) -> Result<(), LiveFeedError> {
        LiveFeedPublisher::announce(self, channel, media_type)
    }

    fn is_watched(&self, channel: &str) -> bool {
        LiveFeedPublisher::is_watched(self, channel)
    }

    fn publish(&self, channel: &str, data: Vec<u8>, capture_time: Option<SystemTime>) -> bool {
        LiveFeedPublisher::publish(self, channel, data, capture_time)
    }
}

/// A time series' summary, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatencyMs {
    pub mean: f32,
    pub p95: f32,
}

/// One message on the stats channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    /// Of this format.
    pub version: u32,
    /// The seconds this report covers.
    pub interval_s: f32,
    pub frames: FrameStats,
    /// By the graph's node ids: every model node, and any other node that
    /// reported an error.
    pub nodes: BTreeMap<String, NodeStats>,
}

/// The frame loop over the interval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameStats {
    /// Frames read from the input per second.
    pub captured_fps: f32,
    /// Frames the graph ran on per second.
    pub processed_fps: f32,
    /// Frames replaced by a newer one before the graph took them.
    pub dropped: u64,
    pub errors: u64,
    /// The graph's time on a frame.
    pub process_ms: Option<LatencyMs>,
    /// From a frame's capture to its results.
    pub latency_ms: Option<LatencyMs>,
}

/// One graph node over the interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStats {
    /// Frames per second it ran on.
    pub fps: f32,
    /// How many times it ran (a per-region node, once per region).
    pub runs: u64,
    /// Frames it didn't run on: sampling, a late frame, or nothing to run on.
    pub skipped: u64,
    /// Of those, frames where it gave way because the frame ran late.
    pub yielded: u64,
    pub errors: u64,
    /// Its time on a frame it ran on, all its runs together.
    pub latency_ms: Option<LatencyMs>,
}

/// A bounded set of samples.
#[derive(Default)]
struct Samples {
    values: Vec<f32>,
    next: usize,
    total: f64,
    count: u64,
}

impl Samples {
    fn push(&mut self, ms: f32) {
        self.total += f64::from(ms);
        self.count += 1;
        if self.values.len() < MAX_SAMPLES {
            self.values.push(ms);
        } else {
            self.values[self.next] = ms;
            self.next = (self.next + 1) % MAX_SAMPLES;
        }
    }

    fn summary(&mut self) -> Option<LatencyMs> {
        if self.count == 0 {
            return None;
        }
        let at = (self.values.len() * 95).div_ceil(100).saturating_sub(1);
        let (_, p95, _) = self.values.select_nth_unstable_by(at, f32::total_cmp);
        Some(LatencyMs {
            mean: (self.total / self.count as f64) as f32,
            p95: *p95,
        })
    }
}

#[derive(Default)]
struct NodeWindow {
    frames: u64,
    runs: u64,
    skipped: u64,
    yielded: u64,
    errors: u64,
    latency: Samples,
}

/// What's added up since the last report.
struct Window {
    start: Instant,
    base: IngestStats,
    frames: u64,
    errors: u64,
    process: Samples,
    latency: Samples,
    nodes: BTreeMap<String, NodeWindow>,
}

impl Window {
    fn new(start: Instant, base: IngestStats, models: &[String]) -> Self {
        Self {
            start,
            base,
            frames: 0,
            errors: 0,
            process: Samples::default(),
            latency: Samples::default(),
            nodes: models
                .iter()
                .map(|id| (id.clone(), NodeWindow::default()))
                .collect(),
        }
    }

    fn add(&mut self, tick: &Tick, process: Option<Duration>, latency: Option<Duration>) {
        self.frames += 1;
        if let Some(d) = process {
            self.process.push(ms(d));
        }
        if let Some(d) = latency {
            self.latency.push(ms(d));
        }
        for (id, node) in &mut self.nodes {
            match tick.timing.get(id).filter(|t| t.runs > 0) {
                Some(t) => {
                    node.frames += 1;
                    node.runs += u64::from(t.runs);
                    node.latency
                        .push(ms(t.time.pre + t.time.infer + t.time.decode));
                }
                None => node.skipped += 1,
            }
        }
        for id in &tick.yielded {
            if let Some(node) = self.nodes.get_mut(id) {
                node.yielded += 1;
            }
        }
        self.errors += tick.errors.len() as u64;
        for error in &tick.errors {
            // `<node>: <what went wrong>`.
            if let Some((id, _)) = error.split_once(": ") {
                match self.nodes.get_mut(id) {
                    Some(node) => node.errors += 1,
                    None => {
                        self.nodes.entry(id.to_string()).or_default().errors += 1;
                    }
                }
            }
        }
    }

    fn report(&mut self, now: Instant, ingest: IngestStats) -> StatsReport {
        let seconds = now.duration_since(self.start).as_secs_f32().max(0.001);
        let per_second = |n: u64| n as f32 / seconds;
        StatsReport {
            version: 1,
            interval_s: seconds,
            frames: FrameStats {
                captured_fps: per_second(ingest.captured.saturating_sub(self.base.captured)),
                processed_fps: per_second(self.frames),
                dropped: ingest.dropped.saturating_sub(self.base.dropped),
                errors: self.errors,
                process_ms: self.process.summary(),
                latency_ms: self.latency.summary(),
            },
            nodes: self
                .nodes
                .iter_mut()
                .map(|(id, n)| {
                    (
                        id.clone(),
                        NodeStats {
                            fps: per_second(n.frames),
                            runs: n.runs,
                            skipped: n.skipped,
                            yielded: n.yielded,
                            errors: n.errors,
                            latency_ms: n.latency.summary(),
                        },
                    )
                })
                .collect(),
        }
    }
}

fn ms(d: Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}

/// What the live view has done since it started; all zero while nobody
/// watches.
#[derive(Default)]
struct Counters {
    frames_counted: AtomicU64,
    stats_sent: AtomicU64,
    frames_queued: AtomicU64,
    frames_replaced: AtomicU64,
    frames_drawn: AtomicU64,
    frames_encoded: AtomicU64,
    frames_sent: AtomicU64,
    frames_too_large: AtomicU64,
}

/// [`Counters`], read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveCounters {
    /// Frames added to the stats.
    pub frames_counted: u64,
    pub stats_sent: u64,
    /// Frames handed to the preview thread (the only reference it takes).
    pub frames_queued: u64,
    /// Of those, replaced by a newer one before it got to them.
    pub frames_replaced: u64,
    pub frames_drawn: u64,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    /// Too large to send at the lowest quality.
    pub frames_too_large: u64,
}

impl Counters {
    fn read(&self) -> LiveCounters {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LiveCounters {
            frames_counted: get(&self.frames_counted),
            stats_sent: get(&self.stats_sent),
            frames_queued: get(&self.frames_queued),
            frames_replaced: get(&self.frames_replaced),
            frames_drawn: get(&self.frames_drawn),
            frames_encoded: get(&self.frames_encoded),
            frames_sent: get(&self.frames_sent),
            frames_too_large: get(&self.frames_too_large),
        }
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A frame for the preview thread: references, no pixels copied.
struct Job {
    frame: Frame,
    tick: Tick,
    capture_time: SystemTime,
}

#[derive(Default)]
struct Slot {
    job: Option<Job>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the preview thread needs to draw.
struct Painter {
    width: u32,
    layouts: Layouts,
    shapes: Arc<Vec<RuleShape>>,
    pixels: Vec<u32>,
    rgb: Vec<u8>,
}

impl Painter {
    /// The frame with what's known about it, as a JPEG.
    fn paint(&mut self, job: &Job, counters: &Counters) -> Option<Vec<u8>> {
        let (fw, fh) = (job.frame.width(), job.frame.height());
        let w = self.width.min(fw).max(1) as usize;
        let h = ((fh as f64 * w as f64 / fw as f64).round() as usize).max(1);
        let scene = Scene {
            frame: job.frame.clone(),
            seq: 0,
            facts: facts::facts(&job.tick, &self.layouts),
            status: vec![],
            recording: None,
            recent: vec![],
            shapes: self.shapes.clone(),
        };
        let view = View {
            map: true,
            panel: false,
            ..View::default()
        };
        self.pixels.resize(w * h, 0);
        annotate(
            &scene,
            &view,
            &mut self.pixels,
            w,
            h,
            if w < 960 { 1 } else { 2 },
        );
        bump(&counters.frames_drawn);
        self.rgb.clear();
        self.rgb.reserve(w * h * 3);
        for p in &self.pixels {
            self.rgb
                .extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
        let jpeg = encode(&self.rgb, w as u32, h as u32, MAX_PREVIEW_BYTES);
        bump(&counters.frames_encoded);
        if jpeg.is_none() {
            bump(&counters.frames_too_large);
        }
        jpeg
    }
}

/// A JPEG of at most `limit` bytes, at the best quality that fits.
fn encode(rgb: &[u8], w: u32, h: u32, limit: usize) -> Option<Vec<u8>> {
    for quality in QUALITIES {
        let mut out = Vec::with_capacity(64 * 1024);
        let done = JpegEncoder::new_with_quality(&mut out, quality).encode(
            rgb,
            w,
            h,
            ExtendedColorType::Rgb8,
        );
        match done {
            Ok(()) if out.len() <= limit => return Some(out),
            Ok(()) => {}
            Err(e) => {
                log::debug!("Couldn't encode a preview frame: {e}");
                return None;
            }
        }
    }
    None
}

fn preview_thread<F: Feed>(
    shared: Arc<Shared>,
    feed: F,
    mut painter: Painter,
    counters: Arc<Counters>,
) {
    loop {
        let job = {
            let mut slot = shared.lock();
            loop {
                if slot.closed {
                    return;
                }
                if let Some(job) = slot.job.take() {
                    break job;
                }
                slot = shared
                    .ready
                    .wait(slot)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // The last viewer may have left while it waited.
        if !feed.is_watched(PREVIEW_CHANNEL) {
            continue;
        }
        if let Some(jpeg) = painter.paint(&job, &counters)
            && feed.publish(PREVIEW_CHANNEL, jpeg, Some(job.capture_time))
        {
            bump(&counters.frames_sent);
        }
    }
}

/// The preview's side of the frame loop.
struct Preview {
    shared: Arc<Shared>,
    every: Duration,
    next: Option<Instant>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        // Not joined: it may be in the middle of a frame, and nothing waits.
        self.shared.lock().closed = true;
        self.shared.ready.notify_all();
    }
}

/// The time of one tick.
#[derive(Clone, Copy, Debug)]
pub struct TickTime {
    pub capture_time: SystemTime,
    /// From [`Live::begin`].
    pub started: Option<Instant>,
    pub now: Instant,
}

/// The live view of one node, driven by its frame loop.
pub struct Live<F: Feed> {
    feed: F,
    stats_on: bool,
    /// Models, by id: the nodes every report lists.
    models: Vec<String>,
    window: Option<Window>,
    preview: Option<Preview>,
    counters: Arc<Counters>,
}

impl<F: Feed> Live<F> {
    /// Announces the channels the setting turns on, and starts the preview
    /// thread if it's one of them.
    pub fn new(config: &LiveConfig, graph: &GraphConfig, feed: F) -> Result<Self, String> {
        let announce = |channel: &str, media_type: &str| {
            feed.announce(channel, media_type)
                .map_err(|e| format!("live feed: {e}"))
        };
        if config.stats {
            announce(STATS_CHANNEL, STATS_MEDIA_TYPE)?;
        }
        let counters = Arc::new(Counters::default());
        let preview = if config.preview.enabled {
            announce(PREVIEW_CHANNEL, PREVIEW_MEDIA_TYPE)?;
            let shared = Arc::new(Shared::default());
            let painter = Painter {
                width: config.preview.width,
                layouts: facts::layouts_of(graph),
                shapes: Arc::new(facts::shapes_of(graph)),
                pixels: vec![],
                rgb: vec![],
            };
            let (thread_shared, thread_feed, thread_counters) =
                (shared.clone(), feed.clone(), counters.clone());
            std::thread::Builder::new()
                .name("cv-live-preview".into())
                .spawn(move || {
                    preview_thread(thread_shared, thread_feed, painter, thread_counters);
                })
                .map_err(|e| format!("live feed: can't start the preview thread: {e}"))?;
            Some(Preview {
                shared,
                every: Duration::from_secs_f32(1.0 / config.preview.fps),
                next: None,
            })
        } else {
            None
        };
        let models = graph
            .nodes
            .iter()
            .filter(|(_, node)| matches!(node, NodeConfig::Model(_)))
            .map(|(id, _)| id.clone())
            .collect();
        Ok(Self {
            feed,
            stats_on: config.stats,
            models,
            window: None,
            preview,
            counters,
        })
    }

    pub fn counters(&self) -> LiveCounters {
        self.counters.read()
    }

    fn stats_watched(&self) -> bool {
        self.stats_on && self.feed.is_watched(STATS_CHANNEL)
    }

    /// Before the graph runs: the clock for the stats, if they're watched.
    pub fn begin(&self) -> Option<Instant> {
        self.stats_watched().then(Instant::now)
    }

    /// After a tick's results went to the emitter. `ingest` is only called
    /// while the stats are watched.
    pub fn on_tick(
        &mut self,
        frame: &Frame,
        tick: Tick,
        time: TickTime,
        ingest: impl Fn() -> IngestStats,
    ) {
        if self.stats_watched() {
            let process = time.started.map(|s| time.now.saturating_duration_since(s));
            let latency = SystemTime::now().duration_since(time.capture_time).ok();
            let models = &self.models;
            let window = self
                .window
                .get_or_insert_with(|| Window::new(time.now, ingest(), models));
            window.add(&tick, process, latency);
            bump(&self.counters.frames_counted);
            self.send_stats(time.now, &ingest);
        } else {
            self.window = None;
        }
        self.offer_preview(frame, tick, time);
    }

    /// No frame this time: the stats still go out, showing the stall.
    pub fn on_idle(&mut self, now: Instant, ingest: impl Fn() -> IngestStats) {
        if self.stats_watched() {
            let models = &self.models;
            self.window
                .get_or_insert_with(|| Window::new(now, ingest(), models));
            self.send_stats(now, &ingest);
        } else {
            self.window = None;
        }
    }

    fn send_stats(&mut self, now: Instant, ingest: &impl Fn() -> IngestStats) {
        let Some(window) = &mut self.window else {
            return;
        };
        if now.saturating_duration_since(window.start) < STATS_EVERY {
            return;
        }
        let current = ingest();
        let report = window.report(now, current);
        *window = Window::new(now, current, &self.models);
        match serde_json::to_vec(&report) {
            Ok(data) => {
                if self
                    .feed
                    .publish(STATS_CHANNEL, data, Some(SystemTime::now()))
                {
                    bump(&self.counters.stats_sent);
                }
            }
            Err(e) => log::debug!("Couldn't serialize the live stats: {e}"),
        }
    }

    fn offer_preview(&mut self, frame: &Frame, tick: Tick, time: TickTime) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        if !self.feed.is_watched(PREVIEW_CHANNEL) {
            return;
        }
        if preview.next.is_some_and(|next| time.now < next) {
            return;
        }
        preview.next = Some(time.now + preview.every);
        let job = Job {
            frame: frame.clone(),
            tick,
            capture_time: time.capture_time,
        };
        // Dropped outside the lock: a replaced frame's results may be large.
        let replaced = preview.shared.lock_replace(job);
        bump(&self.counters.frames_queued);
        if replaced.is_some() {
            bump(&self.counters.frames_replaced);
        }
        preview.shared.ready.notify_one();
    }
}

impl Shared {
    fn lock_replace(&self, job: Job) -> Option<Job> {
        self.lock().job.replace(job)
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
