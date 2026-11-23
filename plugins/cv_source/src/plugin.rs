//! The vision source as an SDK `Source`. It reads frames and runs its graph
//! on them; it emits nothing yet.

use crate::settings::Settings;
use flwrs_cv_source::graph::exec::Plan;
use flwrs_cv_source::ingest::{self, Ingest, Next, Opener};
use flwrs_plugin::plugin::core::{InitializeRequest, SchemaDefinition};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the frame counters are logged.
const STATS_EVERY: Duration = Duration::from_secs(30);

pub(crate) struct VisionSource {
    pub settings: Settings,
    /// The graph, ready to run on frames.
    pub plan: Arc<Mutex<Plan>>,
    /// From loading (models that fell back to the CPU).
    pub warnings: Vec<String>,
    pub sink: Option<LocalSink>,
}

impl Source for VisionSource {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.sink = Some(sink);
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(self.version())
            .with_out_schema(SchemaDefinition::from(self.settings.out_schema.clone())))
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
        let (plan, flag) = (self.plan.clone(), stop.0.clone());
        let read = tokio::task::spawn_blocking(move || read_frames(open, &plan, &flag));
        match read.await {
            Ok(Ok(())) => {
                log::info!("Input ended");
                std::future::pending::<()>().await;
                Ok(())
            }
            Ok(Err(e)) => Err(SourceError::new(e)),
            Err(e) => Err(SourceError::new(format!("the frame loop failed: {e}"))),
        }
    }
}

/// Tells the frame loop to stop when `run` is cancelled.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Runs the graph on the newest frame until the input ends or `stop`.
fn read_frames(open: Opener, plan: &Mutex<Plan>, stop: &AtomicBool) -> Result<(), String> {
    // The plan first: after a reconnect, the previous loop holds it until
    // it has stopped, so two loops never read the camera at once.
    let mut plan = plan.lock().unwrap_or_else(|e| e.into_inner());
    let ingest = Ingest::start(open).map_err(|e| format!("can't start reading: {e}"))?;
    let mut last_stats = Instant::now();
    while !stop.load(Ordering::Acquire) {
        match ingest.next(Duration::from_millis(200)) {
            Next::Frame(item) => {
                let tick = plan.tick_at(&item.captured.frame, item.time.as_secs_f64());
                for error in &tick.errors {
                    log::debug!("Frame {}: {error:?}", item.seq);
                }
            }
            Next::Timeout => {}
            Next::Ended => return Ok(()),
            Next::Failed(e) => return Err(e),
        }
        if last_stats.elapsed() >= STATS_EVERY {
            let s = ingest.stats();
            log::info!(
                "Frames: {} read, {} processed, {} dropped, {} reopens",
                s.captured,
                s.taken,
                s.dropped,
                s.reopened
            );
            last_stats = Instant::now();
        }
    }
    Ok(())
}
