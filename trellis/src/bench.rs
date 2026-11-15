//! `bench`: the plugin's own performance. The fake hub drives the plugin at
//! load and measures timings and resource use. This is the plugin alone, not
//! the whole server (no queues, mappings or other plugins).
//!
//! ```toml
//! # benches/throughput.bench.toml
//! name = "small events"
//! launch_args = "node.json"          # optional
//! payloads = ["payloads/small.json"] # cycled; default: a sample from the in schema
//! warmup_ms = 1000
//! duration_ms = 5000
//! repetitions = 3
//!
//! [load]
//! model = "closed"                   # closed | open | step
//! in_flight = 64                     # closed: events in flight (capped by the plugin's max_unacked)
//! # rate = 1000                      # open: events per second
//! # start_rate = 100, step_rate = 100, step_ms = 2000, max_rate = 100000, max_p99_ms = 50   # step
//! ```
//!
//! Sinks and transforms: latency from send (closed) or from the intended send
//! time (open, step: no coordinated omission) to the plugin's ACK/NACK, so
//! they need the ACKS capability. Sources: events per second against a credit
//! window; with a stimulus (and a correlator, in code), stimulus → event
//! latency. A `load_command` (e.g. `oha`) runs in the background meanwhile.
//!
//! Resources are sampled every 100 ms over the plugin's process tree.

use crate::hub::{Conn, ConnWriter, Fixture, Negotiated, decode};
use crate::payload::{load_with, sample_payload};
use crate::stimulus::{
    CommandStimulus, Correlation, Correlator, Stimulus, StimulusCtx, Vars, free_port, run_command,
    substitute_json_vars, substitute_vars,
};
use crate::target::Target;
use flwrs_plugin::plugin::frame::{PROTOCOL_VERSION, encode_frame, encode_message, read_frame};
use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_plugin::schema::protocol::{
    Ack, Credit, Event, FrameHeader, Initialize, Nack, Ping, Pong, capability::Enum as Capability,
    msg_code::Enum as MsgCode,
};
use hdrhistogram::Histogram;
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

/// A bench file.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct BenchFile {
    pub name: Option<String>,
    pub launch_args: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
    #[serde(default)]
    pub teardown: Vec<Vec<String>>,
    /// Payload files, cycled through (default: a sample from the in schema).
    #[serde(default)]
    pub payloads: Vec<String>,
    #[serde(default = "default_warmup")]
    pub warmup_ms: u64,
    #[serde(default = "default_duration")]
    pub duration_ms: u64,
    #[serde(default = "default_repetitions")]
    pub repetitions: u32,
    #[serde(default)]
    pub load: Load,
    /// Producers: credit granted at once (and returned per event).
    #[serde(default = "default_window")]
    pub credit_window: u32,
    /// Sources: a command fired per event (closed: back to back; open: at the rate).
    pub stimulus: Option<Vec<String>>,
    /// A command run in the background during warm-up and measurement.
    pub load_command: Option<Vec<String>>,
    /// Measure the fake hub against a no-op peer first (its ceiling).
    #[serde(default = "yes")]
    pub calibrate: bool,
    #[serde(skip)]
    pub dir: PathBuf,
}

fn default_warmup() -> u64 {
    1000
}
fn default_duration() -> u64 {
    5000
}
fn default_repetitions() -> u32 {
    3
}
fn default_window() -> u32 {
    64
}
fn yes() -> bool {
    true
}

/// The load model.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Load {
    pub model: Model,
    pub in_flight: u32,
    pub rate: f64,
    pub start_rate: f64,
    pub step_rate: f64,
    pub step_ms: u64,
    pub max_rate: f64,
    pub max_p99_ms: f64,
}

impl Default for Load {
    fn default() -> Self {
        Self {
            model: Model::Closed,
            in_flight: 64,
            rate: 1000.0,
            start_rate: 100.0,
            step_rate: 100.0,
            step_ms: 2000,
            max_rate: 1_000_000.0,
            max_p99_ms: 50.0,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Model {
    /// Keep N events in flight: the maximum throughput.
    #[default]
    Closed,
    /// A fixed rate; latency from the intended send time.
    Open,
    /// Open loop at rising rates until latency or errors cross the limit.
    Step,
}

impl BenchFile {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut file: BenchFile =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        file.dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        if file.name.is_none() {
            file.name = path.file_name().map(|n| {
                n.to_string_lossy()
                    .trim_end_matches(".bench.toml")
                    .to_string()
            });
        }
        Ok(file)
    }

    /// The defaults: closed loop with sample payloads.
    pub fn default_named(name: &str) -> Self {
        toml::from_str::<BenchFile>(&format!("name = {name:?}"))
            .map(|mut f| {
                f.dir = PathBuf::from(".");
                f
            })
            .expect("the defaults parse")
    }
}

/// Latency percentiles, in microseconds.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Percentiles {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

/// Resource use of the plugin's process tree.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Resources {
    /// % of one core.
    pub cpu_avg_pct: f64,
    pub cpu_max_pct: f64,
    pub rss_idle_bytes: u64,
    pub rss_avg_bytes: u64,
    pub rss_max_bytes: u64,
    /// Processes in the tree (max seen).
    pub processes: usize,
}

/// One repetition.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RunStats {
    /// Spawn → Initialize received.
    pub startup_ms: f64,
    /// Events completed in the measured window (answered, or emitted for sources).
    pub events: u64,
    /// NACKs (sinks, transforms) or failed stimuli.
    pub errors: u64,
    pub duration_ms: f64,
    /// Events per second.
    pub throughput: f64,
    pub latency_us: Option<Percentiles>,
    pub resources: Option<Resources>,
}

/// One rate of a step run.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StageStats {
    pub rate: f64,
    pub achieved: f64,
    pub latency_us: Option<Percentiles>,
    pub errors: u64,
    pub ok: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Environment {
    pub os: String,
    pub arch: String,
    pub cpu: String,
    pub cores: usize,
    pub trellis_version: String,
}

/// Medians over the repetitions, with the spread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Summary {
    pub throughput: f64,
    pub throughput_min: f64,
    pub throughput_max: f64,
    pub p50_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub p99_us_max: Option<u64>,
    pub startup_ms: f64,
    pub rss_max_bytes: Option<u64>,
    pub cpu_avg_pct: Option<f64>,
}

/// A bench's results.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BenchResult {
    pub name: String,
    pub plugin: String,
    pub version: String,
    pub model: Model,
    pub environment: Environment,
    /// The fake hub against a no-op peer: the ceiling of this setup.
    pub calibration: Option<RunStats>,
    pub runs: Vec<RunStats>,
    pub summary: Summary,
    /// Step model: one entry per rate.
    pub stages: Vec<StageStats>,
    pub warnings: Vec<String>,
}

/// Runs bench files against a plugin.
pub struct Bencher {
    target: Target,
    stimulus: Option<Box<dyn Stimulus>>,
    correlator: Option<Correlator>,
    vars: Vars,
}

impl Bencher {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            stimulus: None,
            correlator: None,
            vars: Vars::new(),
        }
    }

    /// `${name}` → `value` in payload files, launch args, args and commands.
    pub fn var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// Sources: fired per event (in-process load generation).
    pub fn stimulus(mut self, stimulus: impl Stimulus + 'static) -> Self {
        self.stimulus = Some(Box::new(stimulus));
        self
    }

    /// Sources: finds the stimulus's id in events, for per-event latency.
    pub fn correlate(
        mut self,
        correlate: impl Fn(&PluginPayload) -> Option<Correlation> + Send + Sync + 'static,
    ) -> Self {
        self.correlator = Some(Arc::new(correlate));
        self
    }

    pub async fn run_file(&mut self, path: &Path) -> Result<BenchResult, String> {
        let file = BenchFile::read(path)?;
        self.run(&file).await
    }

    pub async fn run(&mut self, file: &BenchFile) -> Result<BenchResult, String> {
        let port = free_port();
        let target = self.prepare(file, port)?;
        let in_schema = target.in_schema.clone().unwrap_or_default();
        let payloads: Vec<PluginPayload> = if file.payloads.is_empty() {
            vec![sample_payload(&in_schema)]
        } else {
            file.payloads
                .iter()
                .map(|p| {
                    load_with(&file.dir.join(p), &in_schema, |t| {
                        substitute_vars(t, port, &self.vars)
                    })
                })
                .collect::<Result<_, _>>()?
        };
        let payloads = Arc::new(payloads);
        let mut warnings = environment_warnings(&target);
        if file.load.model == Model::Step
            && !target.consumes()
            && self.stimulus.is_none()
            && file.stimulus.is_none()
        {
            return Err("the step model needs events to send: a sink, a transform, or a source with a stimulus".into());
        }

        for command in &file.setup {
            let argv: Vec<String> = command
                .iter()
                .map(|a| substitute_vars(a, port, &self.vars))
                .collect();
            run_command(&argv, &file.dir, port)
                .await
                .map_err(|e| format!("setup: {e}"))?;
        }
        let result = self
            .measure_all(file, &target, payloads, port, &mut warnings)
            .await;
        for command in &file.teardown {
            let argv: Vec<String> = command
                .iter()
                .map(|a| substitute_vars(a, port, &self.vars))
                .collect();
            if let Err(e) = run_command(&argv, &file.dir, port).await {
                warnings.push(format!("teardown: {e}"));
            }
        }
        let (calibration, runs, stages) = result?;
        let summary = summarize(&runs);
        Ok(BenchResult {
            name: file.name.clone().unwrap_or_else(|| "bench".into()),
            plugin: target.manifest.id.clone(),
            version: target.version.clone(),
            model: file.load.model,
            environment: environment(),
            calibration,
            runs,
            summary,
            stages,
            warnings,
        })
    }

    fn prepare(&self, file: &BenchFile, port: u16) -> Result<Target, String> {
        let mut target = self.target.clone();
        if let Some(path) = &file.launch_args {
            let path = file.dir.join(path);
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let args: Json =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            target = target.with_launch_args(substitute_json_vars(&args, port, &self.vars))?;
        } else if !target.launch_args.is_null() {
            let args = substitute_json_vars(&target.launch_args, port, &self.vars);
            target = target.with_launch_args(args)?;
        }
        target.args.extend(
            file.args
                .iter()
                .map(|a| substitute_vars(a, port, &self.vars)),
        );
        target.env.push(("TRELLIS_PORT".into(), port.to_string()));
        target.env.extend(
            file.env
                .iter()
                .map(|(k, v)| (k.clone(), substitute_vars(v, port, &self.vars))),
        );
        Ok(target)
    }

    #[allow(clippy::type_complexity)]
    async fn measure_all(
        &mut self,
        file: &BenchFile,
        target: &Target,
        payloads: Arc<Vec<PluginPayload>>,
        port: u16,
        warnings: &mut Vec<String>,
    ) -> Result<(Option<RunStats>, Vec<RunStats>, Vec<StageStats>), String> {
        let calibration = if file.calibrate && file.load.model != Model::Step {
            let mut fixture = Fixture::without_process(target).await?;
            let peer = tokio::spawn(noop_peer(target.clone(), fixture.port()));
            let accepted = fixture.accepted().await;
            let stats = match accepted {
                Ok((conn, _, negotiated)) => {
                    let mut ctx = RunCtx::new(file, payloads.clone(), port, None, None);
                    let stats = run_once(&mut fixture, conn, negotiated, &mut ctx).await;
                    Some(stats.map(|(s, _)| s)).transpose()
                }
                Err(e) => Err(e),
            };
            peer.abort();
            fixture.stop().await;
            match stats {
                Ok(s) => s,
                Err(e) => {
                    warnings.push(format!("calibration failed: {e}"));
                    None
                }
            }
        } else {
            None
        };

        let mut runs = vec![];
        let mut stages = vec![];
        for _ in 0..file.repetitions.max(1) {
            let mut fixture = Fixture::start(target).await?;
            let (conn, _, negotiated) = fixture.accepted().await?;
            if target.consumes() && !negotiated.acks {
                warnings.push(
                    "the plugin didn't negotiate ACKS: no latency, and throughput is the send rate"
                        .into(),
                );
            }
            let stimulus: Option<&mut Box<dyn Stimulus>> = self.stimulus.as_mut();
            let mut ctx = RunCtx::new(
                file,
                payloads.clone(),
                port,
                stimulus,
                self.correlator.clone(),
            );
            ctx.vars = self.vars.clone();
            let outcome = run_once(&mut fixture, conn, negotiated, &mut ctx).await;
            fixture.stop().await;
            let (stats, run_stages) = outcome?;
            runs.push(stats);
            if stages.is_empty() {
                stages = run_stages;
            }
        }
        warnings.sort();
        warnings.dedup();
        Ok((calibration, runs, stages))
    }
}

/// What one run needs.
struct RunCtx<'a> {
    file: &'a BenchFile,
    /// Send events (sinks, transforms) rather than fire a stimulus (sources).
    consumes: bool,
    payloads: Arc<Vec<PluginPayload>>,
    port: u16,
    vars: Vars,
    stimulus: Option<&'a mut Box<dyn Stimulus>>,
    correlator: Option<Correlator>,
}

impl<'a> RunCtx<'a> {
    fn new(
        file: &'a BenchFile,
        payloads: Arc<Vec<PluginPayload>>,
        port: u16,
        stimulus: Option<&'a mut Box<dyn Stimulus>>,
        correlator: Option<Correlator>,
    ) -> Self {
        Self {
            file,
            consumes: true,
            payloads,
            port,
            vars: Vars::new(),
            stimulus,
            correlator,
        }
    }
}

/// Shared between the sender, the reader and the sampler of one run.
struct Shared {
    /// seq (or correlation id) → (start time, counted).
    pending: Mutex<BTreeMap<u64, (Instant, bool)>>,
    correlated: Mutex<HashMap<String, (Instant, bool)>>,
    hist: Mutex<Histogram<u64>>,
    /// Counted completions (events answered, or emitted by a source).
    done: AtomicU64,
    errors: AtomicU64,
    measuring: AtomicBool,
    stop: AtomicBool,
    last_done: Mutex<Option<Instant>>,
    window: Semaphore,
}

impl Shared {
    fn new(window: usize) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(BTreeMap::new()),
            correlated: Mutex::new(HashMap::new()),
            hist: Mutex::new(new_histogram()),
            done: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            measuring: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            last_done: Mutex::new(None),
            window: Semaphore::new(window),
        })
    }

    fn complete(&self, started: Instant, counted: bool, error: bool) {
        if !counted {
            return;
        }
        if error {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let us = started.elapsed().as_micros().max(1) as u64;
        let _ = self.hist.lock().unwrap().record(us.min(3_600_000_000));
        self.done.fetch_add(1, Ordering::Relaxed);
        *self.last_done.lock().unwrap() = Some(Instant::now());
    }

    fn reset(&self) {
        self.hist.lock().unwrap().reset();
        self.done.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        *self.last_done.lock().unwrap() = None;
    }
}

fn new_histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 3_600_000_000, 3).expect("valid histogram bounds")
}

fn percentiles(hist: &Histogram<u64>) -> Option<Percentiles> {
    (!hist.is_empty()).then(|| Percentiles {
        p50: hist.value_at_quantile(0.50),
        p95: hist.value_at_quantile(0.95),
        p99: hist.value_at_quantile(0.99),
        max: hist.max(),
    })
}

/// One repetition: warm-up, then the measured window (or the step stages).
async fn run_once(
    fixture: &mut Fixture,
    conn: Conn,
    negotiated: Negotiated,
    ctx: &mut RunCtx<'_>,
) -> Result<(RunStats, Vec<StageStats>), String> {
    let startup_ms = fixture.spawned.elapsed().as_secs_f64() * 1000.0;
    let target = fixture.target.clone();
    ctx.consumes = target.consumes();
    let file = ctx.file;
    let window = if target.consumes() && negotiated.acks {
        (file.load.in_flight.max(1)).min(negotiated.max_unacked.max(1)) as usize
    } else {
        Semaphore::MAX_PERMITS
    };
    let shared = Shared::new(window);
    let Conn { reader, writer } = conn;
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    // Resources: idle first, then through the run.
    let sampler = fixture.pid().map(|pid| Sampler::start(pid, shared.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Producers (sources, and transforms for their outputs) need credit.
    if target.produces() && negotiated.credits {
        writer
            .lock()
            .await
            .send(&Credit {
                events: file.credit_window.max(1),
            })
            .await?;
    }
    let reader_task = tokio::spawn(read_loop(
        reader,
        writer.clone(),
        shared.clone(),
        negotiated,
        target.produces(),
        target.consumes(),
        ctx.correlator.clone(),
    ));
    let mut load_child = match &file.load_command {
        Some(argv) => {
            let argv: Vec<String> = argv
                .iter()
                .map(|a| substitute_vars(a, ctx.port, &ctx.vars))
                .collect();
            let (program, args) = argv.split_first().ok_or("load_command is empty")?;
            Some(
                tokio::process::Command::new(program)
                    .args(args)
                    .current_dir(&file.dir)
                    .env("TRELLIS_PORT", ctx.port.to_string())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("load_command: {e}"))?,
            )
        }
        None => None,
    };

    let mut stages = vec![];
    let sending = target.consumes() || ctx.stimulus.is_some() || file.stimulus.is_some();
    let mut seq_counter = 0u64;
    let result = async {
        // Warm-up (not counted).
        if file.warmup_ms > 0 {
            let until = Instant::now() + Duration::from_millis(file.warmup_ms);
            if sending {
                drive(
                    ctx,
                    &writer,
                    &shared,
                    until,
                    rate_for(file, None),
                    &mut seq_counter,
                )
                .await?;
            } else {
                tokio::time::sleep_until(until.into()).await;
            }
        }
        if let Some(s) = &sampler {
            s.mark_load();
        }
        if file.load.model == Model::Step {
            let mut rate = file.load.start_rate.max(1.0);
            while rate <= file.load.max_rate {
                shared.reset();
                shared.measuring.store(true, Ordering::Relaxed);
                let start = Instant::now();
                let until = start + Duration::from_millis(file.load.step_ms.max(100));
                drive(ctx, &writer, &shared, until, Some(rate), &mut seq_counter).await?;
                shared.measuring.store(false, Ordering::Relaxed);
                settle(&shared, Duration::from_secs(2)).await;
                let elapsed = start.elapsed().as_secs_f64();
                let latency = percentiles(&shared.hist.lock().unwrap());
                let errors = shared.errors.load(Ordering::Relaxed);
                let achieved = shared.done.load(Ordering::Relaxed) as f64 / elapsed;
                let ok = errors == 0
                    && latency.is_some_and(|l| l.p99 as f64 <= file.load.max_p99_ms * 1000.0)
                    && achieved >= rate * 0.9;
                stages.push(StageStats {
                    rate,
                    achieved,
                    latency_us: latency,
                    errors,
                    ok,
                });
                if !ok {
                    break;
                }
                rate += file.load.step_rate.max(1.0);
            }
            Ok::<_, String>(None)
        } else {
            shared.reset();
            shared.measuring.store(true, Ordering::Relaxed);
            let start = Instant::now();
            let until = start + Duration::from_millis(file.duration_ms.max(1));
            if sending {
                drive(
                    ctx,
                    &writer,
                    &shared,
                    until,
                    rate_for(file, Some(())),
                    &mut seq_counter,
                )
                .await?;
            } else {
                tokio::time::sleep_until(until.into()).await;
            }
            shared.measuring.store(false, Ordering::Relaxed);
            settle(&shared, Duration::from_secs(5)).await;
            Ok(Some(start))
        }
    }
    .await;
    shared.stop.store(true, Ordering::Relaxed);
    if let Some(child) = &mut load_child {
        let _ = child.start_kill();
    }
    reader_task.abort();
    let resources = match sampler {
        Some(s) => Some(s.finish().await),
        None => None,
    };
    let start = result?;

    let done = shared.done.load(Ordering::Relaxed);
    let duration = match (start, *shared.last_done.lock().unwrap()) {
        (Some(start), Some(last)) if target.consumes() => last.duration_since(start),
        (Some(_), _) => Duration::from_millis(file.duration_ms.max(1)),
        (None, _) => Duration::from_millis(file.load.step_ms * stages.len() as u64),
    };
    let duration_ms = duration.as_secs_f64() * 1000.0;
    let throughput = if duration_ms > 0.0 {
        done as f64 / (duration_ms / 1000.0)
    } else {
        0.0
    };
    let hist = shared.hist.lock().unwrap();
    Ok((
        RunStats {
            startup_ms,
            events: done,
            errors: shared.errors.load(Ordering::Relaxed),
            duration_ms,
            throughput,
            latency_us: percentiles(&hist),
            resources,
        },
        stages,
    ))
}

/// The send rate: `None` = closed loop.
fn rate_for(file: &BenchFile, _measured: Option<()>) -> Option<f64> {
    match file.load.model {
        Model::Closed => None,
        Model::Open => Some(file.load.rate.max(0.001)),
        Model::Step => Some(file.load.start_rate.max(1.0)),
    }
}

/// Waits until nothing counted is in flight (or `limit`).
async fn settle(shared: &Shared, limit: Duration) {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        let waiting = shared.pending.lock().unwrap().values().any(|(_, c)| *c)
            || shared.correlated.lock().unwrap().values().any(|(_, c)| *c);
        if !waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Sends events (sinks, transforms) or fires the stimulus (sources) until
/// `until`: back to back within the window (closed) or at `rate` (open).
async fn drive(
    ctx: &mut RunCtx<'_>,
    writer: &Arc<tokio::sync::Mutex<ConnWriter>>,
    shared: &Arc<Shared>,
    until: Instant,
    rate: Option<f64>,
    counter: &mut u64,
) -> Result<(), String> {
    let start = Instant::now();
    let mut i: u64 = 0;
    let consumes = ctx.consumes;
    let mut command = ctx.file.stimulus.as_ref().map(|argv| CommandStimulus {
        argv: argv.clone(),
        cwd: ctx.file.dir.clone(),
        vars: ctx.vars.clone(),
    });
    while Instant::now() < until {
        let intended = match rate {
            Some(rate) => {
                let at = start + Duration::from_secs_f64(i as f64 / rate);
                if at >= until {
                    break;
                }
                tokio::time::sleep_until(at.into()).await;
                at
            }
            None => Instant::now(),
        };
        let counted = shared.measuring.load(Ordering::Relaxed);
        if consumes {
            // The window: waits while the plugin has max_unacked events.
            let permit = tokio::select! {
                p = shared.window.acquire() => p.map_err(|e| e.to_string())?,
                _ = tokio::time::sleep_until(until.into()) => break,
            };
            permit.forget();
            *counter += 1;
            let payload = ctx.payloads[(*counter as usize) % ctx.payloads.len()].clone();
            let mut w = writer.lock().await;
            let (seq, frame) = w.event_frame(payload, &format!("bench-{counter}"), 1);
            shared
                .pending
                .lock()
                .unwrap()
                .insert(seq, (intended, counted));
            w.raw(&frame).await?;
        } else {
            let sctx = StimulusCtx {
                iteration: *counter,
                intended,
                launch_args: Json::Null,
                port: ctx.port,
            };
            *counter += 1;
            let fired = match (ctx.stimulus.as_mut(), command.as_mut()) {
                (Some(s), _) => s.fire(sctx).await,
                (None, Some(c)) => c.fire(sctx).await,
                (None, None) => Ok(None),
            };
            match fired {
                Ok(Some(id)) => {
                    shared
                        .correlated
                        .lock()
                        .unwrap()
                        .insert(id, (intended, counted));
                }
                Ok(None) => {}
                Err(_) if counted => {
                    shared.errors.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {}
            }
        }
        i += 1;
    }
    Ok(())
}

/// The fake hub's reader: answers, outputs, pings; credit and acks back.
async fn read_loop(
    mut reader: crate::hub::ConnReader,
    writer: Arc<tokio::sync::Mutex<ConnWriter>>,
    shared: Arc<Shared>,
    negotiated: Negotiated,
    produces: bool,
    consumes: bool,
    correlator: Option<Correlator>,
) -> Result<(), String> {
    while !shared.stop.load(Ordering::Relaxed) {
        let Some(frame) = reader.frame(Duration::from_secs(3600)).await? else {
            return Ok(());
        };
        match frame.code() {
            MsgCode::Ack if consumes => {
                let ack: Ack = decode(&frame)?;
                let mut answered = vec![];
                {
                    let mut pending = shared.pending.lock().unwrap();
                    let rest = pending.split_off(&(ack.up_to + 1));
                    answered.extend(std::mem::replace(&mut *pending, rest).into_values());
                    for seq in &ack.seqs {
                        if let Some(entry) = pending.remove(seq) {
                            answered.push(entry);
                        }
                    }
                }
                shared.window.add_permits(answered.len());
                for (started, counted) in answered {
                    shared.complete(started, counted, false);
                }
            }
            MsgCode::Nack if consumes => {
                let nack: Nack = decode(&frame)?;
                let entry = shared.pending.lock().unwrap().remove(&nack.seq);
                if let Some((started, counted)) = entry {
                    shared.window.add_permits(1);
                    shared.complete(started, counted, true);
                }
            }
            MsgCode::Event if produces => {
                let mut w = writer.lock().await;
                if negotiated.credits {
                    w.send(&Credit { events: 1 }).await?;
                }
                if negotiated.acks {
                    w.send(&Ack {
                        up_to: frame.header.seq,
                        seqs: vec![],
                    })
                    .await?;
                }
                drop(w);
                if !consumes {
                    let counted = shared.measuring.load(Ordering::Relaxed);
                    let correlated = correlator.as_ref().and_then(|c| {
                        let event: Event = decode(&frame).ok()?;
                        let id = c(&event.payload.unwrap_or_default())?;
                        shared.correlated.lock().unwrap().remove(&id)
                    });
                    match correlated {
                        Some((started, counted)) => shared.complete(started, counted, false),
                        None if counted && correlator.is_none() => {
                            shared.done.fetch_add(1, Ordering::Relaxed);
                            *shared.last_done.lock().unwrap() = Some(Instant::now());
                        }
                        None => {}
                    }
                }
            }
            MsgCode::Ping => {
                let ping: Ping = decode(&frame)?;
                writer
                    .lock()
                    .await
                    .send(&Pong { nonce: ping.nonce })
                    .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A minimal in-process plugin that does nothing: acks every event at once
/// (consumers) or emits sample events against credit (sources). Measures what
/// the fake hub itself can do.
async fn noop_peer(target: Target, port: u16) -> Result<(), String> {
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let (mut read, mut write) = stream.into_split();
    let init = Initialize {
        instance_id: crate::hub::INSTANCE_ID.into(),
        plugin_version: target.version.clone(),
        plugin_type: target.plugin_type as i32,
        token: String::new(),
        in_schema: target.in_schema.clone(),
        out_schema: target.out_schema.clone(),
        min_protocol_version: PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        capabilities: vec![Capability::Acks as i32, Capability::Credits as i32],
        max_unacked: 1024,
    };
    write
        .write_all(&encode_message(&init))
        .await
        .map_err(|e| e.to_string())?;
    let sample = sample_payload(&target.out_schema.clone().unwrap_or_default());
    let mut seq = 0u64;
    let mut buf = vec![];
    loop {
        let Some(frame) = read_frame(&mut read).await.map_err(|e| e.to_string())? else {
            return Ok(());
        };
        buf.clear();
        match frame.code() {
            MsgCode::Event => buf.extend(encode_message(&Ack {
                up_to: frame.header.seq,
                seqs: vec![],
            })),
            MsgCode::Credit if !target.consumes() => {
                let credit: Credit = decode(&frame)?;
                for _ in 0..credit.events {
                    seq += 1;
                    let header = FrameHeader {
                        code: MsgCode::Event as i32,
                        seq,
                        ..Default::default()
                    };
                    let body = Event {
                        payload: Some(sample.clone()),
                    }
                    .encode_to_vec();
                    buf.extend(encode_frame(&header, &body));
                }
            }
            MsgCode::Ping => {
                let ping: Ping = decode(&frame)?;
                buf.extend(encode_message(&Pong { nonce: ping.nonce }));
            }
            _ => {}
        }
        if !buf.is_empty() {
            write.write_all(&buf).await.map_err(|e| e.to_string())?;
        }
    }
}

/// Samples the plugin's process tree every 100 ms.
struct Sampler {
    task: tokio::task::JoinHandle<Resources>,
    load_started: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Sampler {
    fn start(pid: u32, _shared: Arc<Shared>) -> Self {
        let load_started = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (ls, st) = (load_started.clone(), stop.clone());
        let task = tokio::task::spawn_blocking(move || {
            use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
            let root = Pid::from_u32(pid);
            let Ok(mut sys) = System::new() else {
                return Resources::default();
            };
            let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
            let mut r = Resources::default();
            let (mut cpu_sum, mut rss_sum, mut samples) = (0.0f64, 0u128, 0u64);
            while !st.load(Ordering::Relaxed) {
                sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
                let mut tree = vec![root];
                let mut i = 0;
                while i < tree.len() {
                    let parent = tree[i];
                    tree.extend(
                        sys.processes()
                            .iter()
                            .filter(|(_, p)| p.parent() == Some(parent))
                            .map(|(pid, _)| *pid),
                    );
                    i += 1;
                }
                let procs: Vec<_> = tree.iter().filter_map(|p| sys.process(*p)).collect();
                if procs.is_empty() {
                    break; // the plugin is gone
                }
                let rss: u64 = procs.iter().map(|p| p.memory()).sum();
                let cpu: f64 = procs.iter().map(|p| f64::from(p.cpu_usage())).sum();
                r.processes = r.processes.max(procs.len());
                if ls.load(Ordering::Relaxed) {
                    cpu_sum += cpu;
                    rss_sum += u128::from(rss);
                    samples += 1;
                    r.cpu_max_pct = r.cpu_max_pct.max(cpu);
                    r.rss_max_bytes = r.rss_max_bytes.max(rss);
                } else {
                    r.rss_idle_bytes = rss;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if samples > 0 {
                r.cpu_avg_pct = cpu_sum / samples as f64;
                r.rss_avg_bytes = (rss_sum / u128::from(samples)) as u64;
            }
            r
        });
        Self {
            task,
            load_started,
            stop,
        }
    }

    fn mark_load(&self) {
        self.load_started.store(true, Ordering::Relaxed);
    }

    async fn finish(self) -> Resources {
        self.stop.store(true, Ordering::Relaxed);
        self.task.await.unwrap_or_default()
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

fn summarize(runs: &[RunStats]) -> Summary {
    let throughput: Vec<f64> = runs.iter().map(|r| r.throughput).collect();
    let p = |f: fn(&Percentiles) -> u64| -> Vec<f64> {
        runs.iter()
            .filter_map(|r| r.latency_us.as_ref().map(|l| f(l) as f64))
            .collect()
    };
    let opt = |v: Vec<f64>| (!v.is_empty()).then(|| median(v) as u64);
    let res: Vec<&Resources> = runs.iter().filter_map(|r| r.resources.as_ref()).collect();
    Summary {
        throughput: median(throughput.clone()),
        throughput_min: throughput
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .min(f64::MAX),
        throughput_max: throughput.iter().copied().fold(0.0, f64::max),
        p50_us: opt(p(|l| l.p50)),
        p99_us: opt(p(|l| l.p99)),
        p99_us_max: p(|l| l.p99).into_iter().map(|v| v as u64).max(),
        startup_ms: median(runs.iter().map(|r| r.startup_ms).collect()),
        rss_max_bytes: res.iter().map(|r| r.rss_max_bytes).max(),
        cpu_avg_pct: (!res.is_empty()).then(|| median(res.iter().map(|r| r.cpu_avg_pct).collect())),
    }
}

fn environment() -> Environment {
    use sysinfo::{CpuRefreshKind, RefreshKind, System};
    let sys =
        System::new_with_specifics(RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing())).ok();
    let cpus = sys.as_ref().map(|s| s.cpus()).unwrap_or_default();
    Environment {
        os: System::long_os_version().unwrap_or_else(|_| std::env::consts::OS.into()),
        arch: std::env::consts::ARCH.into(),
        cpu: cpus
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_default(),
        cores: cpus.len(),
        trellis_version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// Conditions that make numbers unreliable.
fn environment_warnings(target: &Target) -> Vec<String> {
    let mut warnings = vec![];
    let program = target.program.to_string_lossy();
    if program.contains("/debug/") || program.contains("\\debug\\") {
        warnings.push("the plugin looks like a debug build".into());
    }
    if let Ok(governor) =
        std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        && governor.trim() != "performance"
    {
        warnings.push(format!(
            "CPU frequency governor is {:?}, not \"performance\": numbers vary with CPU scaling",
            governor.trim()
        ));
    }
    warnings
}

/// A table for people.
pub fn report(results: &[BenchResult]) -> String {
    let mut out = String::new();
    for r in results {
        out.push_str(&format!(
            "{} ({} {}, {:?} loop)\n",
            r.name, r.plugin, r.version, r.model
        ));
        let line = |label: &str, s: &RunStats| {
            let lat = s
                .latency_us
                .map(|l| {
                    format!(
                        "p50 {} µs  p95 {} µs  p99 {} µs  max {} µs",
                        l.p50, l.p95, l.p99, l.max
                    )
                })
                .unwrap_or_else(|| "no latency".into());
            let res = s
                .resources
                .as_ref()
                .map(|r| {
                    format!(
                        "  cpu {:.0}% (max {:.0}%)  rss {} idle / {} max",
                        r.cpu_avg_pct,
                        r.cpu_max_pct,
                        mib(r.rss_idle_bytes),
                        mib(r.rss_max_bytes)
                    )
                })
                .unwrap_or_default();
            format!(
                "  {label:<12} {:>10.0} ev/s  {lat}  errors {}  startup {:.0} ms{res}\n",
                s.throughput, s.errors, s.startup_ms
            )
        };
        if let Some(c) = &r.calibration {
            out.push_str(&line("calibration", c));
        }
        for (i, run) in r.runs.iter().enumerate() {
            out.push_str(&line(&format!("run {}", i + 1), run));
        }
        let s = &r.summary;
        out.push_str(&format!(
            "  median       {:>10.0} ev/s  (min {:.0}, max {:.0})  p99 {}\n",
            s.throughput,
            s.throughput_min,
            s.throughput_max,
            s.p99_us
                .map(|v| format!("{v} µs"))
                .unwrap_or_else(|| "-".into())
        ));
        for st in &r.stages {
            out.push_str(&format!(
                "  rate {:>8.0}  achieved {:>8.0}  p99 {:>8}  errors {}  {}\n",
                st.rate,
                st.achieved,
                st.latency_us
                    .map(|l| format!("{} µs", l.p99))
                    .unwrap_or_else(|| "-".into()),
                st.errors,
                if st.ok { "ok" } else { "LIMIT" }
            ));
        }
        for w in &r.warnings {
            out.push_str(&format!("  warning: {w}\n"));
        }
        out.push('\n');
    }
    out
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Regressions against a baseline (results matched by name): throughput
/// lower, or p99 latency higher, by more than `max_pct` percent.
pub fn compare(baseline: &[BenchResult], current: &[BenchResult], max_pct: f64) -> Vec<String> {
    let mut regressions = vec![];
    for c in current {
        let Some(b) = baseline.iter().find(|b| b.name == c.name) else {
            continue;
        };
        let (bt, ct) = (b.summary.throughput, c.summary.throughput);
        if bt > 0.0 && (bt - ct) / bt * 100.0 > max_pct {
            regressions.push(format!(
                "{}: throughput {ct:.0} ev/s vs {bt:.0} ({:.1}% lower)",
                c.name,
                (bt - ct) / bt * 100.0
            ));
        }
        if let (Some(bp), Some(cp)) = (b.summary.p99_us, c.summary.p99_us)
            && bp > 0
            && (cp as f64 - bp as f64) / bp as f64 * 100.0 > max_pct
        {
            regressions.push(format!(
                "{}: p99 {cp} µs vs {bp} µs ({:.1}% higher)",
                c.name,
                (cp as f64 - bp as f64) / bp as f64 * 100.0
            ));
        }
    }
    regressions
}
