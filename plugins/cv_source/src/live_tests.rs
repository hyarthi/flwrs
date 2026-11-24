use super::*;
use crate::graph::exec::NodeTiming;
use crate::model::Timing;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

/// A channel and what was published on it.
type Sent = (String, Vec<u8>);

/// A feed that keeps what it's given; `hold` makes `publish` wait.
#[derive(Clone, Default)]
struct TestFeed {
    announced: Arc<Mutex<Vec<(String, String)>>>,
    watched: Arc<Mutex<BTreeSet<String>>>,
    sent: Arc<Mutex<Vec<Sent>>>,
    hold: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl TestFeed {
    fn watch(&self, channels: &[&str]) {
        *self.watched.lock().unwrap() = channels.iter().map(|c| c.to_string()).collect();
    }

    fn sent(&self, channel: &str) -> Vec<Vec<u8>> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| c == channel)
            .map(|(_, d)| d.clone())
            .collect()
    }
}

impl Feed for TestFeed {
    fn announce(&self, channel: &str, media_type: &str) -> Result<(), LiveFeedError> {
        self.announced
            .lock()
            .unwrap()
            .push((channel.into(), media_type.into()));
        Ok(())
    }

    fn is_watched(&self, channel: &str) -> bool {
        self.watched.lock().unwrap().contains(channel)
    }

    fn publish(&self, channel: &str, data: Vec<u8>, _: Option<SystemTime>) -> bool {
        while self.hold.load(Ordering::Acquire) {
            self.held.store(true, Ordering::Release);
            std::thread::yield_now();
        }
        self.sent.lock().unwrap().push((channel.into(), data));
        true
    }
}

fn graph() -> GraphConfig {
    let json = serde_json::json!({
        "nodes": {
            "camera": { "type": "source" },
            "det": {
                "type": "model", "model": "a.onnx",
                "pre": { "width": 8, "height": 8, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw" },
                "decode": { "type": "detections_e2e", "output": "rows" }
            }
        },
        "links": [{ "from": "camera.frame", "to": "det.frame" }]
    });
    crate::graph::parse(&json).unwrap().0
}

fn config(stats: bool, preview: bool) -> LiveConfig {
    LiveConfig {
        stats,
        preview: PreviewConfig {
            enabled: preview,
            ..PreviewConfig::default()
        },
    }
}

fn tick(runs: u32) -> Tick {
    let mut tick = Tick::default();
    tick.timing.insert(
        "det".into(),
        NodeTiming {
            runs,
            time: Timing {
                pre: Duration::from_millis(1),
                infer: Duration::from_millis(2),
                decode: Duration::from_millis(1),
            },
        },
    );
    tick
}

fn at(now: Instant) -> TickTime {
    TickTime {
        capture_time: SystemTime::now(),
        started: Some(now),
        now,
    }
}

fn ingest() -> IngestStats {
    IngestStats::default()
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn settings_default_off_and_are_range_checked() {
    let off: LiveConfig = serde_json::from_str("{}").unwrap();
    assert!(!off.any());
    assert_eq!((off.preview.fps, off.preview.width), (5.0, 640));
    assert!(off.problems().is_empty());
    let on: LiveConfig = serde_json::from_str(
        r#"{"stats": true, "preview": {"enabled": true, "fps": 15, "width": 960}}"#,
    )
    .unwrap();
    assert!(on.any() && on.problems().is_empty());
    for bad in [
        r#"{"preview": {"fps": 0}}"#,
        r#"{"preview": {"fps": 31}}"#,
        r#"{"preview": {"width": 100}}"#,
        r#"{"preview": {"width": 4000}}"#,
    ] {
        let parsed: LiveConfig = serde_json::from_str(bad).unwrap();
        assert_eq!(parsed.problems().len(), 1, "{bad}");
    }
    assert!(serde_json::from_str::<LiveConfig>(r#"{"unknown": 1}"#).is_err());
}

#[test]
fn only_the_channels_turned_on_are_announced() {
    let announced = |stats, preview| {
        let feed = TestFeed::default();
        let _live = Live::new(&config(stats, preview), &graph(), feed.clone()).unwrap();
        let list = feed.announced.lock().unwrap().clone();
        list.into_iter().map(|(c, _)| c).collect::<Vec<_>>()
    };
    assert!(announced(false, false).is_empty());
    assert_eq!(announced(true, false), ["stats"]);
    assert_eq!(announced(false, true), ["preview"]);
    assert_eq!(announced(true, true), ["stats", "preview"]);
}

/// With nobody watching: no counting, drawing, encoding, serializing, and no
/// reference to the frame kept.
#[test]
fn nothing_is_done_while_nobody_watches() {
    let feed = TestFeed::default();
    let mut live = Live::new(&config(true, true), &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    let start = Instant::now();
    let asked = AtomicU64::new(0);
    for n in 0..200 {
        let now = start + Duration::from_millis(50 * n);
        assert_eq!(live.begin(), None);
        live.on_tick(&frame, tick(1), at(now), || {
            asked.fetch_add(1, Ordering::Relaxed);
            ingest()
        });
    }
    assert_eq!(live.counters(), LiveCounters::default());
    assert_eq!(asked.load(Ordering::Relaxed), 0);
    assert!(feed.sent.lock().unwrap().is_empty());
    let shared = &live.preview.as_ref().unwrap().shared;
    assert!(shared.lock().job.is_none(), "no frame handed over");
}

#[test]
fn a_channel_turned_off_in_the_settings_stays_quiet_even_if_watched() {
    let feed = TestFeed::default();
    feed.watch(&[STATS_CHANNEL, PREVIEW_CHANNEL]);
    let mut live = Live::new(&config(false, false), &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    let start = Instant::now();
    for n in 0..100 {
        live.on_tick(
            &frame,
            tick(1),
            at(start + Duration::from_millis(100 * n)),
            ingest,
        );
    }
    assert_eq!(live.counters(), LiveCounters::default());
    assert!(feed.sent.lock().unwrap().is_empty());
}

#[test]
fn stats_are_sent_about_once_a_second_keyed_by_node() {
    let feed = TestFeed::default();
    feed.watch(&[STATS_CHANNEL]);
    let mut live = Live::new(&config(true, false), &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    let start = Instant::now();
    let captured = AtomicU64::new(0);
    let stats = || {
        let n = captured.fetch_add(3, Ordering::Relaxed);
        IngestStats {
            captured: n,
            dropped: n / 3,
            ..IngestStats::default()
        }
    };
    // 10 frames per second for 3.05 s; every other frame the model skips,
    // one frame fails, one frame yields.
    for n in 0..=30u32 {
        let mut t = tick(u32::from(n % 2 == 0));
        if n == 3 {
            t.errors.push("det: bad output".into());
            t.yielded.push("det".into());
        }
        let now = start + Duration::from_millis(u64::from(n) * 100 + 50);
        live.on_tick(&frame, t, at(now), stats);
    }
    let sent = feed.sent(STATS_CHANNEL);
    assert_eq!(sent.len(), 3, "one report per second");
    let raw: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    for key in ["version", "interval_s", "frames", "nodes"] {
        assert!(raw.get(key).is_some(), "{key}: {raw}");
    }
    for key in [
        "captured_fps",
        "processed_fps",
        "dropped",
        "errors",
        "process_ms",
        "latency_ms",
    ] {
        assert!(raw["frames"].get(key).is_some(), "{key}: {raw}");
    }
    for key in ["fps", "runs", "skipped", "yielded", "errors", "latency_ms"] {
        assert!(raw["nodes"]["det"].get(key).is_some(), "{key}: {raw}");
    }
    assert!(raw["nodes"].get("camera").is_none(), "no timing, no entry");
    let first: StatsReport = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(first.version, 1);
    assert!(
        (first.interval_s - 1.0).abs() < 0.11,
        "{}",
        first.interval_s
    );
    assert_eq!(first.frames.errors, 1);
    let det = &first.nodes["det"];
    assert_eq!((det.errors, det.yielded), (1, 1));
    assert_eq!(det.runs + det.skipped, 11);
    assert!((det.latency_ms.unwrap().mean - 4.0).abs() < 0.01);
    assert!((det.latency_ms.unwrap().p95 - 4.0).abs() < 0.01);
    assert!(first.frames.processed_fps > 9.0 && first.frames.processed_fps < 12.0);
    assert!(first.frames.captured_fps > 0.0);
}

#[test]
fn a_percentile_comes_from_the_kept_samples() {
    let mut s = Samples::default();
    assert_eq!(s.summary(), None);
    for n in 1..=100 {
        s.push(n as f32);
    }
    let summary = s.summary().unwrap();
    assert_eq!(summary.p95, 95.0);
    assert!((summary.mean - 50.5).abs() < 1e-3);
    let mut bounded = Samples::default();
    for n in 0..(MAX_SAMPLES * 3) {
        bounded.push(n as f32);
    }
    assert_eq!(bounded.values.len(), MAX_SAMPLES);
}

#[test]
fn the_stats_restart_when_watching_resumes() {
    let feed = TestFeed::default();
    feed.watch(&[STATS_CHANNEL]);
    let mut live = Live::new(&config(true, false), &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(8, 8, [0, 0, 0]).unwrap();
    let start = Instant::now();
    live.on_tick(&frame, tick(1), at(start), ingest);
    feed.watch(&[]);
    live.on_tick(&frame, tick(1), at(start + Duration::from_secs(5)), ingest);
    assert!(live.window.is_none());
    feed.watch(&[STATS_CHANNEL]);
    live.on_tick(&frame, tick(1), at(start + Duration::from_secs(6)), ingest);
    assert!(feed.sent(STATS_CHANNEL).is_empty(), "a fresh interval");
    assert_eq!(live.window.as_ref().unwrap().frames, 1);
}

#[test]
fn a_stalled_input_still_reports() {
    let feed = TestFeed::default();
    feed.watch(&[STATS_CHANNEL]);
    let mut live = Live::new(&config(true, false), &graph(), feed.clone()).unwrap();
    let start = Instant::now();
    live.on_idle(start, ingest);
    live.on_idle(start + Duration::from_millis(1200), ingest);
    let sent = feed.sent(STATS_CHANNEL);
    assert_eq!(sent.len(), 1);
    let report: StatsReport = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(report.frames.processed_fps, 0.0);
}

#[test]
fn the_preview_is_a_jpeg_at_the_set_width_capped_at_its_rate() {
    let feed = TestFeed::default();
    feed.watch(&[PREVIEW_CHANNEL]);
    let mut settings = config(false, true);
    settings.preview.width = 200;
    let mut live = Live::new(&settings, &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(800, 400, [200, 40, 40]).unwrap();
    let start = Instant::now();
    // 30 frames per second for a second, at 5 per second.
    for n in 0..30u64 {
        live.on_tick(
            &frame,
            tick(1),
            at(start + Duration::from_millis(n * 1000 / 30)),
            ingest,
        );
        // Each one is taken before the next, so none is replaced.
        let shared = live.preview.as_ref().unwrap().shared.clone();
        wait_for("the slot to empty", || shared.lock().job.is_none());
    }
    assert_eq!(live.counters().frames_queued, 5);
    wait_for("five frames", || feed.sent(PREVIEW_CHANNEL).len() == 5);
    let image = image::load_from_memory_with_format(
        &feed.sent(PREVIEW_CHANNEL)[0],
        image::ImageFormat::Jpeg,
    )
    .unwrap()
    .to_rgb8();
    assert_eq!(image.dimensions(), (200, 100));
    let p = image.get_pixel(100, 50);
    assert!(p[0] > 170 && p[1] < 80 && p[2] < 80, "{p:?}");
}

#[test]
fn a_small_frame_is_not_enlarged() {
    let feed = TestFeed::default();
    feed.watch(&[PREVIEW_CHANNEL]);
    let mut live = Live::new(&config(false, true), &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    live.on_tick(&frame, tick(1), at(Instant::now()), ingest);
    wait_for("a frame", || !feed.sent(PREVIEW_CHANNEL).is_empty());
    let image = image::load_from_memory(&feed.sent(PREVIEW_CHANNEL)[0]).unwrap();
    assert_eq!((image.width(), image.height()), (64, 32));
}

/// The preview thread stuck in `publish`: the frame loop goes on, and only
/// the newest frame waits.
#[test]
fn a_stuck_preview_never_holds_up_the_frame_loop() {
    let feed = TestFeed::default();
    feed.watch(&[PREVIEW_CHANNEL]);
    feed.hold.store(true, Ordering::Release);
    let mut live = Live::new(&config(false, true), &graph(), feed.clone()).unwrap();
    let frame = Frame::filled(64, 32, [1, 2, 3]).unwrap();
    let start = Instant::now();
    live.on_tick(&frame, tick(1), at(start), ingest);
    wait_for("the thread to be held", || {
        feed.held.load(Ordering::Acquire)
    });
    let began = Instant::now();
    for n in 1..=50u64 {
        live.on_tick(&frame, tick(1), at(start + Duration::from_secs(n)), ingest);
    }
    assert!(began.elapsed() < Duration::from_secs(2));
    let counters = live.counters();
    assert_eq!(counters.frames_queued, 51);
    assert_eq!(counters.frames_replaced, 49, "one running, one waiting");
    feed.hold.store(false, Ordering::Release);
    wait_for("the last frame", || feed.sent(PREVIEW_CHANNEL).len() == 2);
}

#[test]
fn a_frame_over_the_limit_lowers_quality_or_is_left_out() {
    // Noise compresses badly: quality matters.
    let (w, h) = (256u32, 256u32);
    let mut seed = 0x1234_5678u32;
    let rgb: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    let best = encode(&rgb, w, h, usize::MAX).unwrap().len();
    let lower = encode(&rgb, w, h, best - 1).unwrap();
    assert!(lower.len() < best);
    assert!(encode(&rgb, w, h, 100).is_none());
}

#[test]
fn the_preview_thread_ends_with_the_view() {
    let feed = TestFeed::default();
    let live = Live::new(&config(false, true), &graph(), feed).unwrap();
    let shared = live.preview.as_ref().unwrap().shared.clone();
    drop(live);
    assert!(shared.lock().closed);
    wait_for("the thread to let go", || Arc::strong_count(&shared) == 1);
}
