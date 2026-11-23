//! The debug command: its settings, the frame loop, the console output.

use super::compare::compare;
use super::compose::Scene;
use super::facts::{self, Layouts};
use super::record::{Recorded, Recorder};
use crate::backend::Device;
use crate::graph::config::{Execution, GraphConfig, NodeConfig};
use crate::graph::exec::{self, Plan, Tick};
use crate::graph::{self, Checked};
use crate::ingest::config::{CameraConfig, FileInput, InputConfig};
use crate::ingest::{self, Ingest, Next};
use clap::Parser;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Run a node's graph without a hub and show what it sees
#[derive(Parser, Debug)]
#[command(name = "flwrs-cv-source debug")]
pub struct DebugArgs {
    /// JSON file: the node's launch args (`graph`, optional `input`, `execution`)
    #[arg(long, conflicts_with = "node")]
    pub settings: Option<PathBuf>,

    /// A node's folder: reads its `config/` (graph, input, execution, files)
    #[arg(long)]
    pub node: Option<PathBuf>,

    /// A folder with the files the graph names (models, labels); repeatable
    #[arg(long = "files")]
    pub files_dirs: Vec<PathBuf>,

    /// A file the graph names; repeatable
    #[arg(long = "file")]
    pub files: Vec<PathBuf>,

    /// Read this camera instead (index, id or part of its name; empty: the first)
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with_all = ["video", "images"])]
    pub camera: Option<String>,

    /// Read this MJPEG file instead
    #[arg(long, conflicts_with = "images")]
    pub video: Option<PathBuf>,

    /// Read this folder of JPEG / PNG images instead
    #[arg(long)]
    pub images: Option<PathBuf>,

    /// Frames per second, for `--video` and `--images` (default: every frame, none dropped)
    #[arg(long)]
    pub fps: Option<f32>,

    /// Start over at the end of `--video` or `--images`
    #[arg(long = "loop")]
    pub looped: bool,

    /// No window: the detections on the console
    #[arg(long)]
    pub no_window: bool,

    /// Console output as JSON lines (implies --no-window)
    #[arg(long)]
    pub json: bool,

    /// Console: print a frame only when what's seen changes
    #[arg(long)]
    pub on_change: bool,

    /// Console: leave out times, so runs on the same frames can be compared
    #[arg(long)]
    pub no_timing: bool,

    /// Run each model on the CPU as well and show where results differ
    #[arg(long)]
    pub compare_cpu: bool,

    /// Record from the start (in a window, `r` starts and stops it)
    #[arg(long)]
    pub record: bool,

    /// Where recordings go
    #[arg(long, default_value = ".data/recordings")]
    pub record_dir: PathBuf,

    /// Stop after this many frames
    #[arg(long)]
    pub max_frames: Option<u64>,

    /// Cores the models may use (default: the runtime's choice)
    #[arg(long, default_value_t = 0)]
    pub threads: usize,
}

/// What the run needs, read from the settings.
pub(super) struct Loaded {
    graph: GraphConfig,
    checked: Checked,
    files: BTreeMap<String, PathBuf>,
    execution: Execution,
    input: InputConfig,
}

fn read_value(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn add_dir(dir: &Path, files: &mut BTreeMap<String, PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            files.entry(name.to_string()).or_insert(path);
        }
    }
    Ok(())
}

impl DebugArgs {
    /// The graph, input and execution as a node's settings give them.
    fn node_settings(&self) -> Result<(Value, Option<Value>, Option<Value>), Vec<String>> {
        let one = |e: String| vec![e];
        if let Some(path) = &self.settings {
            let mut all = read_value(path).map_err(one)?;
            let graph = all
                .get_mut("graph")
                .map(Value::take)
                .ok_or_else(|| vec![format!("{}: no `graph`", path.display())])?;
            let input = all.get_mut("input").map(Value::take);
            let execution = all.get_mut("execution").map(Value::take);
            return Ok((graph, input, execution));
        }
        let Some(node) = &self.node else {
            return Err(vec![
                "give the node's settings: --settings <file> or --node <folder>".into(),
            ]);
        };
        let config = node.join("config");
        let optional = |name: &str| {
            let path = config.join(name);
            path.exists()
                .then(|| read_value(&path))
                .transpose()
                .map_err(one)
        };
        let graph = read_value(&config.join("graph.json")).map_err(one)?;
        Ok((graph, optional("input.json")?, optional("execution.json")?))
    }

    fn input_override(&self) -> Option<InputConfig> {
        let file = |path: &PathBuf| FileInput {
            path: path.display().to_string(),
            fps: self.fps,
            looped: self.looped,
        };
        if let Some(device) = &self.camera {
            return Some(InputConfig::Camera(CameraConfig {
                device: (!device.is_empty()).then(|| device.clone()),
                ..CameraConfig::default()
            }));
        }
        if let Some(path) = &self.video {
            return Some(InputConfig::Mjpeg(file(path)));
        }
        self.images.as_ref().map(|p| InputConfig::Images(file(p)))
    }

    /// Everything the run needs, or every problem found.
    pub(super) fn load(&self) -> Result<Loaded, Vec<String>> {
        let (graph_json, input_json, execution_json) = self.node_settings()?;
        let mut problems = vec![];
        let parsed = graph::parse(&graph_json)
            .map_err(|p| problems.extend(p))
            .ok();
        let input = match self.input_override() {
            Some(input) => Some(input),
            None => match input_json {
                Some(v) => serde_json::from_value::<InputConfig>(v)
                    .map_err(|e| problems.push(format!("input: {e}")))
                    .ok(),
                None => {
                    problems.push(
                        "no input: give --camera, --video or --images, or an `input` in the settings"
                            .into(),
                    );
                    None
                }
            },
        };
        if let Some(i) = &input {
            problems.extend(i.problems());
        }
        let execution = match execution_json {
            None => Some(Execution::default()),
            Some(v) => match serde_json::from_value::<Execution>(v) {
                Ok(e) => {
                    problems.extend(e.problems());
                    Some(e)
                }
                Err(e) => {
                    problems.push(format!("execution: {e}"));
                    None
                }
            },
        };
        let mut files = BTreeMap::new();
        for path in &self.files {
            match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => {
                    files.insert(name.to_string(), path.clone());
                }
                None => problems.push(format!("--file {}: not a file name", path.display())),
            }
        }
        if let Some(node) = &self.node
            && let Err(e) = add_dir(&node.join("config"), &mut files)
        {
            problems.push(e);
        }
        for dir in &self.files_dirs {
            if let Err(e) = add_dir(dir, &mut files) {
                problems.push(e);
            }
        }
        if let Some((graph, _)) = &parsed {
            for name in graph::uploads(graph) {
                if !files.contains_key(&name) {
                    problems.push(format!(
                        "the graph names the file {name:?}: point --files at its folder, or give it with --file"
                    ));
                }
            }
        }
        match (parsed, input, execution) {
            (Some((graph, checked)), Some(input), Some(execution)) if problems.is_empty() => {
                Ok(Loaded {
                    graph,
                    checked,
                    files,
                    execution,
                    input,
                })
            }
            _ => Err(problems),
        }
    }

    fn wants_window(&self) -> bool {
        !self.no_window && !self.json && cfg!(feature = "debug-window") && has_display()
    }
}

/// Whether a window can open here.
fn has_display() -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|d| !d.is_empty()))
}

/// The same graph with every model on the CPU.
fn on_cpu(graph: &GraphConfig, defaults: &Execution) -> (GraphConfig, Execution) {
    let cpu = |e: &Execution| Execution {
        runtime: e.runtime,
        device: Some(Device::Cpu),
        provider: None,
        provider_options: BTreeMap::new(),
        allow_fallback: None,
    };
    let mut out = graph.clone();
    for node in out.nodes.values_mut() {
        if let NodeConfig::Model(m) = node {
            let own = m.execution.clone().unwrap_or_default().over(defaults);
            m.execution = Some(cpu(&own));
        }
    }
    (out, cpu(defaults))
}

/// Events kept on screen.
const RECENT_EVENTS: usize = 6;

/// Tells the frame loop what to do: shared with the window's keys.
#[derive(Default)]
pub(super) struct Control {
    pub stop: AtomicBool,
    pub paused: AtomicBool,
    /// Frames to run while paused.
    pub steps: AtomicU32,
    /// Whether to be recording.
    pub record: AtomicBool,
}

/// Frames per second, smoothed.
#[derive(Default)]
struct Rate {
    last: Option<Instant>,
    fps: f32,
}

impl Rate {
    fn tick(&mut self) -> f32 {
        let now = Instant::now();
        if let Some(last) = self.last.replace(now) {
            let current = 1.0 / now.duration_since(last).as_secs_f32().max(1e-6);
            self.fps = if self.fps == 0.0 {
                current
            } else {
                self.fps * 0.9 + current * 0.1
            };
        }
        self.fps
    }
}

fn status(tick: &Tick, fps: f32, dropped: u64, devices: &[(String, String)]) -> Vec<String> {
    let mut out = vec![format!("{fps:.1} fps, {dropped} frames dropped")];
    for (id, device) in devices {
        let time = tick.timing.get(id).map_or(String::new(), |t| {
            let ms = (t.time.pre + t.time.infer + t.time.decode).as_secs_f32() * 1000.0;
            format!(" {ms:.1} ms x{}", t.runs)
        });
        out.push(format!("{id}:{time} on {device}"));
    }
    out
}

/// What the run produced, for its exit status.
pub(super) enum Ended {
    /// The input ended, or the window closed.
    Done,
    Failed(String),
}

/// Reads frames, runs the graph and hands each scene to `show` (false: stop).
fn frame_loop(
    loaded: &Loaded,
    args: &DebugArgs,
    mut plan: Plan,
    mut cpu: Option<Plan>,
    control: &Control,
    mut show: impl FnMut(Scene, Vec<String>) -> bool,
) -> Ended {
    let workdir = std::env::temp_dir();
    let open = ingest::opener(&loaded.input, &loaded.files, &workdir);
    let ingest = match Ingest::start(open) {
        Ok(ingest) => ingest,
        Err(e) => return Ended::Failed(format!("can't start reading: {e}")),
    };
    let layouts: Layouts = facts::layouts_of(&loaded.graph);
    let devices = plan.devices();
    let shapes = Arc::new(facts::shapes_of(&loaded.graph));
    let mut recent = std::collections::VecDeque::new();
    let mut rate = Rate::default();
    let mut shown = 0u64;
    plan.set_deterministic(loaded.input.is_deterministic());
    if let Some(cpu) = &mut cpu {
        cpu.set_deterministic(loaded.input.is_deterministic());
    }
    let mut recorder: Option<Recorder> = None;
    let mut record_failed = false;
    while !control.stop.load(Ordering::Acquire) {
        let wanted = control.record.load(Ordering::Acquire);
        if wanted && recorder.is_none() && !record_failed {
            match Recorder::start(&args.record_dir) {
                Ok(r) => {
                    let tap = r.tap();
                    ingest.set_tap(Some(Box::new(move |f| tap.push(f))));
                    recorder = Some(r);
                }
                Err(e) => {
                    eprintln!("can't record: {e}");
                    record_failed = true;
                }
            }
        } else if !wanted {
            record_failed = false;
            if let Some(r) = recorder.take() {
                ingest.set_tap(None);
                report_recording(r.stop());
            }
        }
        if control.paused.load(Ordering::Acquire) {
            let steps = control.steps.load(Ordering::Acquire);
            let took_step = steps > 0
                && control
                    .steps
                    .compare_exchange(steps, steps - 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok();
            if !took_step {
                std::thread::sleep(Duration::from_millis(15));
                continue;
            }
        }
        let item = match ingest.next(Duration::from_millis(200)) {
            Next::Frame(item) => item,
            Next::Timeout => continue,
            Next::Ended => {
                finish_recording(&ingest, &mut recorder);
                return Ended::Done;
            }
            Next::Failed(e) => {
                finish_recording(&ingest, &mut recorder);
                return Ended::Failed(e);
            }
        };
        let frame = item.captured.frame;
        let time = item.time.as_secs_f64();
        let tick = plan.tick_at(&frame, time);
        let mut differences = vec![];
        if let Some(cpu) = &mut cpu {
            let other = cpu.tick_at(&frame, time);
            for (node, result) in &tick.results {
                if let Some((_, theirs)) = other.results.iter().find(|(n, _)| n == node) {
                    compare(node, result, theirs, &mut differences);
                }
            }
        }
        let mut frame_facts = facts::facts(&tick, &layouts);
        frame_facts
            .notes
            .extend(differences.iter().map(|d| format!("vs CPU: {d}")));
        for e in &frame_facts.events {
            if recent.len() == RECENT_EVENTS {
                recent.pop_front();
            }
            recent.push_back(facts::event_line(e));
        }
        let fps = rate.tick();
        let scene = Scene {
            frame,
            seq: item.seq,
            facts: frame_facts,
            status: status(&tick, fps, ingest.stats().dropped, &devices),
            recording: recorder.as_ref().map(Recorder::elapsed),
            recent: recent.iter().cloned().collect(),
            shapes: shapes.clone(),
        };
        if !show(scene, differences) {
            break;
        }
        shown += 1;
        if args.max_frames.is_some_and(|max| shown >= max) {
            break;
        }
    }
    finish_recording(&ingest, &mut recorder);
    Ended::Done
}

fn finish_recording(ingest: &Ingest, recorder: &mut Option<Recorder>) {
    if let Some(r) = recorder.take() {
        ingest.set_tap(None);
        report_recording(r.stop());
    }
}

fn report_recording(r: Recorded) {
    if let Some(e) = &r.error {
        eprintln!("recording failed: {e}");
    }
    eprintln!(
        "recorded {} frames ({:.1} s, {} dropped) to {}\nplay it back with: --video {} --fps {:.1}",
        r.frames,
        r.seconds,
        r.dropped,
        r.path.display(),
        r.path.display(),
        r.fps()
    );
}

/// Prints scenes on the console.
pub(super) struct Console<W: Write> {
    out: W,
    json: bool,
    on_change: bool,
    timing: bool,
    last: Option<String>,
}

impl<W: Write> Console<W> {
    fn new(out: W, args: &DebugArgs) -> Self {
        Self {
            out,
            json: args.json,
            on_change: args.on_change,
            timing: !args.no_timing,
            last: None,
        }
    }

    /// False when the output is gone (a closed pipe).
    fn print(&mut self, scene: &Scene) -> bool {
        if self.on_change {
            let signature = facts::signature(&scene.facts);
            if self.last.as_ref() == Some(&signature) {
                return true;
            }
            self.last = Some(signature);
        }
        let (w, h) = (scene.frame.width(), scene.frame.height());
        let written = if self.json {
            let mut value = facts::to_json(&scene.facts);
            value["frame"] = scene.seq.into();
            value["width"] = w.into();
            value["height"] = h.into();
            if self.timing {
                value["status"] = scene.status.clone().into();
            }
            writeln!(self.out, "{value}")
        } else {
            let mut text = format!(
                "frame {} {w}x{h}: {} objects",
                scene.seq,
                scene.facts.objects.len()
            );
            if self.timing {
                text.push_str(" | ");
                text.push_str(&scene.status.join(" | "));
            }
            text.push('\n');
            for line in facts::text_lines(&scene.facts) {
                text.push_str(&line);
                text.push('\n');
            }
            self.out.write_all(text.as_bytes())
        };
        written.and_then(|()| self.out.flush()).is_ok()
    }
}

fn compile(loaded: &Loaded, args: &DebugArgs) -> Result<(Plan, Option<Plan>), Vec<String>> {
    let (plan, warnings) = exec::compile(
        &loaded.graph,
        &loaded.checked,
        &loaded.files,
        &loaded.execution,
        args.threads,
    )?;
    warnings.iter().for_each(|w| eprintln!("warning: {w}"));
    for (id, device) in plan.devices() {
        eprintln!("{id} runs on {device}");
    }
    let cpu = if args.compare_cpu {
        let (graph, defaults) = on_cpu(&loaded.graph, &loaded.execution);
        let (plan, _) = exec::compile(
            &graph,
            &loaded.checked,
            &loaded.files,
            &defaults,
            args.threads,
        )
        .map_err(|p| {
            p.into_iter()
                .map(|e| format!("on the CPU: {e}"))
                .collect::<Vec<_>>()
        })?;
        Some(plan)
    } else {
        None
    };
    Ok((plan, cpu))
}

fn run_console(loaded: &Loaded, args: &DebugArgs, plan: Plan, cpu: Option<Plan>) -> Ended {
    let control = Control::default();
    control.record.store(args.record, Ordering::Release);
    let mut console = Console::new(std::io::stdout().lock(), args);
    frame_loop(loaded, args, plan, cpu, &control, |scene, _| {
        console.print(&scene)
    })
}

/// A run that hasn't started, handed back when no window can open.
#[cfg(feature = "debug-window")]
struct Unstarted {
    loaded: Loaded,
    args: DebugArgs,
    plan: Plan,
    cpu: Option<Plan>,
}

#[cfg(feature = "debug-window")]
fn run_window(run: Unstarted) -> Result<Ended, (String, Box<Unstarted>)> {
    let event_loop = match super::window::event_loop() {
        Ok(event_loop) => event_loop,
        Err(e) => return Err((e, Box::new(run))),
    };
    let Unstarted {
        loaded,
        args,
        plan,
        cpu,
    } = run;
    let control = Arc::new(Control::default());
    control.record.store(args.record, Ordering::Release);
    let mailbox = Arc::new(super::window::Mailbox::default());
    let waker = event_loop.create_proxy();
    let (worker_control, worker_mail) = (control.clone(), mailbox.clone());
    let worker = std::thread::Builder::new()
        .name("cv-debug".into())
        .spawn(move || {
            // The window stays open on the last frame until it's closed.
            frame_loop(&loaded, &args, plan, cpu, &worker_control, |scene, _| {
                worker_mail.put(scene);
                waker.send_event(()).is_ok()
            })
        });
    let worker = match worker {
        Ok(worker) => worker,
        Err(e) => return Ok(Ended::Failed(format!("can't start the frame loop: {e}"))),
    };
    let shown = super::window::show(event_loop, mailbox, control.clone());
    control.stop.store(true, Ordering::Release);
    if let Err(e) = shown {
        return Ok(Ended::Failed(e));
    }
    Ok(worker
        .join()
        .unwrap_or_else(|_| Ended::Failed("the frame loop panicked".into())))
}

/// Log records on stderr: the input's retries and warnings.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

/// `flwrs-cv-source debug …`; `argv` starts at `debug`.
pub fn main(argv: impl IntoIterator<Item = String>) -> ExitCode {
    let args = DebugArgs::parse_from(argv);
    if log::set_logger(&StderrLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let fail = |problems: Vec<String>| {
        problems.iter().for_each(|p| eprintln!("{p}"));
        ExitCode::from(2)
    };
    let loaded = match args.load() {
        Ok(loaded) => loaded,
        Err(problems) => return fail(problems),
    };
    let (plan, cpu) = match compile(&loaded, &args) {
        Ok(plans) => plans,
        Err(problems) => return fail(problems),
    };
    if !args.no_window && !args.json && !cfg!(feature = "debug-window") {
        eprintln!("this build has no window (feature debug-window): printing to the console");
    }
    let ended = if args.wants_window() {
        windowed(loaded, args, plan, cpu)
    } else {
        run_console(&loaded, &args, plan, cpu)
    };
    match ended {
        Ended::Done => ExitCode::SUCCESS,
        Ended::Failed(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "debug-window")]
fn windowed(loaded: Loaded, args: DebugArgs, plan: Plan, cpu: Option<Plan>) -> Ended {
    let run = Unstarted {
        loaded,
        args,
        plan,
        cpu,
    };
    match run_window(run) {
        Ok(ended) => ended,
        Err((e, run)) => {
            eprintln!("can't open a window ({e}): printing to the console");
            let Unstarted {
                loaded,
                args,
                plan,
                cpu,
            } = *run;
            run_console(&loaded, &args, plan, cpu)
        }
    }
}

#[cfg(not(feature = "debug-window"))]
fn windowed(loaded: Loaded, args: DebugArgs, plan: Plan, cpu: Option<Plan>) -> Ended {
    run_console(&loaded, &args, plan, cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, v) in [("a.png", 10u8), ("b.png", 200)] {
            image::RgbImage::from_pixel(16, 8, image::Rgb([v; 3]))
                .save(dir.path().join(name))
                .unwrap();
        }
        dir
    }

    fn args(list: &[&str]) -> DebugArgs {
        DebugArgs::parse_from(std::iter::once("debug").chain(list.iter().copied()))
    }

    fn settings(dir: &Path, body: Value) -> String {
        let path = dir.join("settings.json");
        std::fs::write(&path, body.to_string()).unwrap();
        path.to_str().unwrap().to_string()
    }

    const SOURCE_ONLY: &str = r#"{"nodes": {"camera": {"type": "source"}}}"#;

    fn console_output(a: &DebugArgs) -> String {
        let loaded = a.load().unwrap();
        let (plan, cpu) = compile(&loaded, a).unwrap();
        let mut out = vec![];
        {
            let mut console = Console::new(&mut out, a);
            let control = Control::default();
            let ended = frame_loop(&loaded, a, plan, cpu, &control, |s, _| console.print(&s));
            assert!(matches!(ended, Ended::Done));
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_frame_folder_prints_a_line_per_frame() {
        let dir = folder();
        let graph: Value = serde_json::from_str(SOURCE_ONLY).unwrap();
        let s = settings(dir.path(), serde_json::json!({"graph": graph}));
        let images = dir.path().to_str().unwrap();
        let a = args(&[
            "--settings",
            &s,
            "--images",
            images,
            "--no-window",
            "--no-timing",
        ]);
        assert_eq!(
            console_output(&a),
            "frame 0 16x8: 0 objects\nframe 1 16x8: 0 objects\n"
        );
    }

    #[test]
    fn json_lines_and_max_frames() {
        let dir = folder();
        let graph: Value = serde_json::from_str(SOURCE_ONLY).unwrap();
        let s = settings(dir.path(), serde_json::json!({"graph": graph}));
        let images = dir.path().to_str().unwrap();
        let a = args(&[
            "--settings",
            &s,
            "--images",
            images,
            "--json",
            "--no-timing",
            "--max-frames",
            "1",
        ]);
        let out = console_output(&a);
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(
            (v["frame"].as_u64(), v["width"].as_u64()),
            (Some(0), Some(16))
        );
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn on_change_prints_only_what_changes() {
        let dir = folder();
        let graph: Value = serde_json::from_str(SOURCE_ONLY).unwrap();
        let s = settings(dir.path(), serde_json::json!({"graph": graph}));
        let images = dir.path().to_str().unwrap();
        let a = args(&[
            "--settings",
            &s,
            "--images",
            images,
            "--no-window",
            "--no-timing",
            "--on-change",
        ]);
        assert_eq!(console_output(&a), "frame 0 16x8: 0 objects\n");
    }

    #[test]
    fn a_node_folder_gives_its_settings_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("graph.json"), SOURCE_ONLY).unwrap();
        std::fs::write(
            config.join("input.json"),
            r#"{"type": "images", "path": "frames"}"#,
        )
        .unwrap();
        std::fs::write(config.join("model.onnx"), b"x").unwrap();
        let a = args(&["--node", dir.path().to_str().unwrap(), "--no-window"]);
        let loaded = a.load().unwrap();
        assert!(matches!(loaded.input, InputConfig::Images(_)));
        assert!(loaded.files.contains_key("model.onnx"));
    }

    #[test]
    fn problems_are_all_reported() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(
            dir.path(),
            serde_json::json!({"graph": {"nodes": {"Bad": {"type": "source"}}}}),
        );
        let problems = args(&["--settings", &s]).load().err().unwrap();
        assert!(
            problems.iter().any(|p| p.contains("no input")),
            "{problems:?}"
        );
        assert!(problems.len() >= 2, "{problems:?}");
        let none = args(&[]).load().err().unwrap();
        assert!(none[0].contains("--settings"), "{none:?}");
    }

    #[test]
    fn the_cpu_copy_forces_every_model_to_the_cpu() {
        let mut example: Value =
            serde_json::from_str(include_str!("../../examples/graphs/mobilenet_v2.json")).unwrap();
        let nodes = example["graph"]["nodes"].as_object_mut().unwrap();
        let id = nodes
            .iter()
            .find(|(_, n)| n["type"] == "model")
            .map(|(id, _)| id.clone())
            .unwrap();
        nodes[&id]["execution"] = serde_json::json!({"device": "gpu", "provider": "coreml"});
        let graph: GraphConfig = serde_json::from_value(example["graph"].take()).unwrap();
        let (cpu, defaults) = on_cpu(
            &graph,
            &Execution {
                device: Some(Device::Npu),
                ..Execution::default()
            },
        );
        assert_eq!(defaults.device, Some(Device::Cpu));
        let NodeConfig::Model(m) = &cpu.nodes[&id] else {
            panic!()
        };
        let e = m.execution.as_ref().unwrap();
        assert_eq!((e.device, e.provider.as_deref()), (Some(Device::Cpu), None));
    }
}
