//! The vision source as an SDK `Source`: reads frames, runs its graph on them
//! and sends what the `emit` settings say, through an outbox, so the frame
//! loop never waits for the hub.

use crate::settings::Settings;
use flwrs_cv_source::emit::{Emission, Emitter, FrameInfo, Outbox, Outgoing};
use flwrs_cv_source::graph::exec::Plan;
use flwrs_cv_source::ingest::{self, Ingest, Next, Opener};
use flwrs_cv_source::live::{Live, TickTime};
use flwrs_plugin::plugin::core::Unconfirmed;
use flwrs_plugin::plugin::core::{InitializeRequest, SchemaDefinition};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::plugin::live::LiveFeedPublisher;
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition as OutSchema};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;
use flwrs_types::value::coerce_payload;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the frame counters are logged.
const STATS_EVERY: Duration = Duration::from_secs(30);
/// How soon a rule event the hub can't take yet is sent again.
const RETRY_EVENT: Duration = Duration::from_millis(250);

pub(crate) struct VisionSource {
    pub settings: Settings,
    /// The graph, ready to run on frames.
    pub plan: Arc<Mutex<Plan>>,
    /// From loading (models that fell back to the CPU).
    pub warnings: Vec<String>,
    pub sink: Option<LocalSink>,
    /// Kept across reconnects: what wasn't confirmed goes out again.
    pub outbox: Arc<Outbox<PluginPayload>>,
    pub emitter: Arc<Mutex<Emitter>>,
    /// The live view, when the `live` setting turns any of it on.
    pub live: Option<(LiveFeedPublisher, Arc<Mutex<Live<LiveFeedPublisher>>>)>,
}

impl Source for VisionSource {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.sink = Some(sink);
        let request = InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(self.version())
            .with_out_schema(SchemaDefinition::from(self.settings.out_schema.clone()));
        Ok(match &self.live {
            Some((feed, _)) => request.with_live_feed(feed.clone()),
            None => request,
        })
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    async fn run(&self) -> Result<(), SourceError> {
        for w in &self.warnings {
            log::warn!("{w}");
        }
        let devices: Vec<String> = match self.plan.lock() {
            Ok(plan) => plan
                .devices()
                .into_iter()
                .map(|(id, d)| format!("{id} on {d}"))
                .collect(),
            Err(_) => vec![],
        };
        log::info!(
            "Graph of {} nodes loaded ({}); models: {}",
            self.settings.graph.nodes.len(),
            self.settings.checked.order.join(" → "),
            if devices.is_empty() {
                "none".to_string()
            } else {
                devices.join(", ")
            }
        );
        let Some(input) = &self.settings.input else {
            log::info!("No input configured: reading no frames");
            // Cancelled by the runner on shutdown or a disconnect.
            std::future::pending::<()>().await;
            return Ok(());
        };
        let workdir = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let open = ingest::opener(input, &self.settings.files, &workdir);
        if let Ok(mut plan) = self.plan.lock() {
            plan.set_deterministic(input.is_deterministic());
        }
        let stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
        let job = FrameJob {
            plan: self.plan.clone(),
            emitter: self.emitter.clone(),
            outbox: self.outbox.clone(),
            schema: self.settings.out_schema.clone(),
            live: self.live.as_ref().map(|(_, live)| live.clone()),
            stop: stop.0.clone(),
        };
        let sink = self
            .sink
            .clone()
            .ok_or_else(|| SourceError::new("no sink: the source wasn't initialized"))?;
        let mut read = tokio::task::spawn_blocking(move || read_frames(open, &job));
        let deliver = deliver(&sink, &self.outbox);
        tokio::pin!(deliver);
        tokio::select! {
            result = &mut deliver => result,
            finished = &mut read => {
                match finished {
                    Ok(Ok(())) => log::info!("Input ended"),
                    Ok(Err(e)) => return Err(SourceError::new(e)),
                    Err(e) => return Err(SourceError::new(format!("the frame loop failed: {e}"))),
                }
                // What's queued still goes out.
                deliver.await
            }
        }
    }
}

/// Sends what the outbox holds. A rule event is sent until the hub takes it
/// or refuses it for good; cancelling this (a disconnect) loses none of them.
async fn deliver(sink: &LocalSink, outbox: &Outbox<PluginPayload>) -> Result<(), SourceError> {
    loop {
        match outbox.next().await {
            Outgoing::Frame(payload) => sink.emit(payload).await?,
            Outgoing::Event(payload) => match sink.emit_tracked(payload).await?.confirmed().await {
                Ok(()) => outbox.event_done(),
                Err(Unconfirmed::Refused {
                    reason,
                    retry: false,
                }) => {
                    log::warn!("The hub refused a rule event: {reason}");
                    outbox.event_done();
                }
                Err(Unconfirmed::Refused { retry: true, .. }) => {
                    tokio::time::sleep(RETRY_EVENT).await;
                }
                Err(e @ Unconfirmed::Lost) => {
                    return Err(SourceError::new(e.to_string()));
                }
            },
        }
    }
}

/// What the frame loop works with.
struct FrameJob {
    plan: Arc<Mutex<Plan>>,
    emitter: Arc<Mutex<Emitter>>,
    outbox: Arc<Outbox<PluginPayload>>,
    schema: OutSchema,
    live: Option<Arc<Mutex<Live<LiveFeedPublisher>>>>,
    stop: Arc<AtomicBool>,
}

/// Tells the frame loop to stop when `run` is cancelled.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Runs the graph on the newest frame until the input ends or `stop`.
fn read_frames(open: Opener, job: &FrameJob) -> Result<(), String> {
    // The plan first: after a reconnect, the previous loop holds it until
    // it has stopped, so two loops never read the camera at once.
    let mut plan = job.plan.lock().unwrap_or_else(|e| e.into_inner());
    let mut emitter = job.emitter.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = job
        .live
        .as_ref()
        .map(|live| live.lock().unwrap_or_else(|e| e.into_inner()));
    let ingest = Ingest::start(open).map_err(|e| format!("can't start reading: {e}"))?;
    let mut last_stats = Instant::now();
    while !job.stop.load(Ordering::Acquire) {
        match ingest.next(Duration::from_millis(200)) {
            Next::Frame(item) => {
                let time = item.time.as_secs_f64();
                let frame = &item.captured.frame;
                let started = live.as_ref().and_then(|live| live.begin());
                let tick = plan.tick_at(frame, time);
                for error in &tick.errors {
                    log::debug!("Frame {}: {error:?}", item.seq);
                }
                let info = FrameInfo {
                    seq: item.seq + 1,
                    capture_time: item.captured.capture_time,
                    width: frame.width(),
                    height: frame.height(),
                };
                for emission in emitter.on_tick(&tick, &info, time) {
                    send(&job.outbox, &job.schema, emission);
                }
                if let Some(live) = &mut live {
                    let time = TickTime {
                        capture_time: item.captured.capture_time,
                        started,
                        now: Instant::now(),
                    };
                    live.on_tick(frame, tick, time, || ingest.stats());
                }
            }
            Next::Timeout => {
                if let Some(live) = &mut live {
                    live.on_idle(Instant::now(), || ingest.stats());
                }
            }
            Next::Ended => return Ok(()),
            Next::Failed(e) => return Err(e),
        }
        if last_stats.elapsed() >= STATS_EVERY {
            let s = ingest.stats();
            let o = job.outbox.stats();
            log::info!(
                "Frames: {} read, {} processed, {} dropped, {} reopens; sent {} rule events ({} dropped), {} frame results ({} replaced)",
                s.captured,
                s.taken,
                s.dropped,
                s.reopened,
                o.events_sent,
                o.events_dropped,
                o.frames_sent,
                o.frames_replaced
            );
            last_stats = Instant::now();
        }
    }
    Ok(())
}

/// Shapes a value to the node's schema and queues it.
fn send(outbox: &Outbox<PluginPayload>, schema: &OutSchema, emission: Emission) {
    let (value, is_event) = match emission {
        Emission::Event(v) => (v, true),
        Emission::Frame(v) => (v, false),
    };
    let payload = coerce_payload(value, schema).and_then(|v| v.into_payload());
    match payload {
        Ok(p) if is_event => outbox.push_event(p),
        Ok(p) => outbox.push_frame(p),
        Err(e) => log::warn!("An event doesn't fit the node's schema: {e}"),
    }
}
