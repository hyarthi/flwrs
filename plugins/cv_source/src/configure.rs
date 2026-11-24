//! The configurator (manifest `configurator`; see `docs/plugin-spec.md`).
//!
//! When a node is saved it writes the node's `out_schema` (from the graph's
//! `emit` settings) and `model_info` (what each model file says about
//! itself), and reports what wouldn't work on this machine as warnings: a
//! scene may be configured here and run elsewhere. In `--mode preflight` the
//! same probe's findings are problems, permanent or retryable.
//!
//! It never opens a camera: listing one disturbs nobody, including a running
//! version of this node, so what `releasing` nodes hold needs no special care.

use crate::emit;
use crate::graph::config::{Execution, GraphConfig, ModelConfig, NodeConfig};
use crate::graph::{self, Checked, exec};
use crate::ingest::camera;
use crate::ingest::config::InputConfig;
use crate::inspect::{self, ModelInfo};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

/// How long model loads may take, inside the server's time for a run.
const LOAD_BUDGET: Duration = Duration::from_secs(20);

/// Something that keeps the node from running here.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Problem {
    pub message: String,
    /// It may go away by itself (a camera not plugged in yet).
    pub retryable: bool,
}

impl Problem {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

struct Node {
    graph: GraphConfig,
    checked: Checked,
    execution: Execution,
    input: Option<InputConfig>,
    files: BTreeMap<String, PathBuf>,
}

/// The node's settings. Refusals are about the settings themselves, so they
/// hold on any machine.
fn parse(args: &Value, folder: &Path) -> Result<Node, Vec<String>> {
    let mut problems = vec![];
    let setting = |key: &str| args.get(key).filter(|v| !v.is_null()).cloned();
    let graph = match setting("graph") {
        Some(json) => graph::parse(&json).map_err(|p| problems.extend(p)).ok(),
        None => {
            problems.push("graph: missing".to_string());
            None
        }
    };
    let execution = match setting("execution") {
        None => Some(Execution::default()),
        Some(json) => match serde_json::from_value::<Execution>(json) {
            Ok(e) => {
                let own = e.problems();
                let ok = own.is_empty();
                problems.extend(own);
                ok.then_some(e)
            }
            Err(e) => {
                problems.push(format!("execution: {e}"));
                None
            }
        },
    };
    let input = match setting("input") {
        None => None,
        Some(json) => match serde_json::from_value::<InputConfig>(json) {
            Ok(input) => {
                problems.extend(input.problems());
                Some(input)
            }
            Err(e) => {
                problems.push(format!("input: {e}"));
                None
            }
        },
    };
    let files = args
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|name| {
            let path = folder.join("config").join(name);
            path.is_file().then(|| (name.to_string(), path))
        })
        .collect();
    match (graph, execution) {
        (Some((graph, checked)), Some(execution)) if problems.is_empty() => Ok(Node {
            graph,
            checked,
            execution,
            input,
            files,
        }),
        _ => Err(problems),
    }
}

/// What a model file's description says against its node's settings.
fn fit_warnings(id: &str, model: &ModelConfig, info: &ModelInfo) -> Vec<String> {
    let mut out = vec![];
    let decode = serde_json::to_value(&model.decode).unwrap_or(Value::Null);
    let named: Vec<&str> = [
        "output",
        "logits",
        "boxes",
        "scores",
        "dets",
        "x",
        "y",
        "presence",
        "world",
        "handedness",
    ]
    .iter()
    .chain(&["keypoints"])
    .filter_map(|key| decode.get(*key).and_then(Value::as_str))
    .chain(
        ["outputs", "cls", "reg"]
            .iter()
            .filter_map(|key| decode.get(*key).and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str),
    )
    .collect();
    for name in named {
        if !info.outputs.iter().any(|p| p.name == name) {
            let have: Vec<&str> = info.outputs.iter().map(|p| p.name.as_str()).collect();
            out.push(format!(
                "node {id}: {} has no output {name:?} (it has: {})",
                model.model,
                have.join(", ")
            ));
        }
    }
    let input = match &model.input_name {
        Some(name) => info.inputs.iter().find(|p| &p.name == name),
        None => info.inputs.first(),
    };
    if let (Some(pre), Some(input)) = (&model.pre, input) {
        let size = match (pre.layout, &input.shape[..]) {
            (crate::pre::Layout::Nchw, [_, _, h, w]) | (crate::pre::Layout::Nhwc, [_, h, w, _]) => {
                Some((*h, *w))
            }
            _ => None,
        };
        if let Some((h, w)) = size
            && (h.is_some_and(|h| h != i64::from(pre.height))
                || w.is_some_and(|w| w != i64::from(pre.width)))
        {
            out.push(format!(
                "node {id}: pre is {}x{}, but {} takes {}x{}",
                pre.width,
                pre.height,
                model.model,
                w.map_or("any".into(), |v| v.to_string()),
                h.map_or("any".into(), |v| v.to_string()),
            ));
        }
    }
    out
}

/// Loads every model on its runtime and device, on a thread so that a slow
/// load can't use up the server's time for this run.
fn load_models(node: &Node) -> (Vec<Problem>, Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let (graph, checked, files, execution) = (
        node.graph.clone(),
        node.checked.clone(),
        node.files.clone(),
        node.execution.clone(),
    );
    std::thread::spawn(move || {
        // Few threads, since a running scene may share this machine; two
        // where there are two, so a background detector plans as it will.
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(2));
        let result = exec::compile_probe(&graph, &checked, &files, &execution, threads)
            .map(|(_, warnings)| warnings);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(LOAD_BUDGET) {
        Ok(Ok(warnings)) => (vec![], warnings),
        Ok(Err(problems)) => (
            problems.into_iter().map(Problem::permanent).collect(),
            vec![],
        ),
        Err(_) => (
            vec![],
            vec![format!(
                "the models weren't all loaded within {} s, so they're only checked when the node starts",
                LOAD_BUDGET.as_secs()
            )],
        ),
    }
}

fn check_input(node: &Node) -> Result<(), Problem> {
    match &node.input {
        None => Ok(()),
        Some(InputConfig::Camera(camera)) => {
            camera::find(camera).map(|_| ()).map_err(|e| Problem {
                message: format!("input: {}", e.message),
                retryable: e.retryable,
            })
        }
        Some(InputConfig::Images(f) | InputConfig::Mjpeg(f)) => {
            let path = Path::new(&f.path);
            // A relative path that's no node file is in the node's folder,
            // which this run can't see.
            if node.files.contains_key(&f.path) || !path.is_absolute() || path.exists() {
                Ok(())
            } else {
                Err(Problem::permanent(format!("input: no file at {}", f.path)))
            }
        }
    }
}

struct Probe {
    problems: Vec<Problem>,
    warnings: Vec<String>,
    models: BTreeMap<String, ModelInfo>,
}

fn probe(node: &Node) -> Probe {
    let mut p = Probe {
        problems: vec![],
        warnings: vec![],
        models: BTreeMap::new(),
    };
    let missing: Vec<String> = graph::uploads(&node.graph)
        .into_iter()
        .filter(|name| !node.files.contains_key(name))
        .collect();
    for name in &missing {
        p.problems.push(Problem::permanent(format!(
            "the graph names the file {name:?}, which isn't among the node's files (upload it and add it to files)"
        )));
    }
    let mut readable = missing.is_empty();
    for (id, n) in &node.graph.nodes {
        let NodeConfig::Model(model) = n else {
            continue;
        };
        let Some(path) = node.files.get(&model.model) else {
            continue;
        };
        if !p.models.contains_key(&model.model) {
            match inspect::inspect(path) {
                Ok(info) => {
                    p.models.insert(model.model.clone(), info);
                }
                Err(e) => {
                    readable = false;
                    p.problems
                        .push(Problem::permanent(format!("{}: {e}", model.model)));
                    continue;
                }
            }
        }
        p.warnings
            .extend(fit_warnings(id, model, &p.models[&model.model]));
    }
    if let Err(problem) = check_input(node) {
        p.problems.push(problem);
    }
    p.warnings.extend(core_note(&node.graph));
    if readable {
        let (problems, warnings) = load_models(node);
        p.problems.extend(problems);
        p.warnings.extend(warnings);
    }
    p
}

/// A note when the execution groups ask for more cores than this machine
/// has. Only a warning: the node may run elsewhere, and the configured
/// values don't depend on the host.
fn core_note(graph: &GraphConfig) -> Option<String> {
    let asked = graph::groups::thread_sum(graph);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    (asked > cores).then(|| {
        format!("the execution groups ask for {asked} threads, and this machine has {cores} cores")
    })
}

/// The answer to a save: the configured values, and warnings.
pub fn configure(args: &Value, folder: &Path) -> Result<(Value, Vec<String>), Vec<String>> {
    let node = parse(args, folder)?;
    let probe = probe(&node);
    let mut warnings: Vec<String> = probe.problems.into_iter().map(|p| p.message).collect();
    warnings.extend(probe.warnings);
    if emit::config_of(&node.graph).mode == crate::graph::config::EmitMode::Rules
        && !node
            .graph
            .nodes
            .values()
            .any(|n| matches!(n, NodeConfig::Rule(_) | NodeConfig::Lua(_)))
    {
        warnings.push(
            "the graph has no rules and emits only rule events, so it sends nothing: add a rule, or an emit node with mode changes or frames".into(),
        );
    }
    let values = json!({
        "out_schema": emit::out_schema(&node.graph),
        "model_info": probe.models,
    });
    Ok((values, warnings))
}

/// The answer to a pre-flight check: can this node run here now?
pub fn preflight(args: &Value, folder: &Path) -> (Vec<Problem>, Vec<String>) {
    match parse(args, folder) {
        Err(problems) => (
            problems.into_iter().map(Problem::permanent).collect(),
            vec![],
        ),
        Ok(node) => {
            let probe = probe(&node);
            (probe.problems, probe.warnings)
        }
    }
}

/// What the node's uploaded files say about themselves, whatever state the
/// graph is in: an editor's proposals for a picked file. A file that isn't a
/// model (labels, a charset) is left out.
pub fn describe(args: &Value, folder: &Path) -> Value {
    let models: BTreeMap<String, ModelInfo> = args
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|name| {
            let path = folder.join("config").join(name);
            path.is_file()
                .then(|| inspect::inspect(&path).ok())
                .flatten()
                .map(|info| (name.to_string(), info))
        })
        .collect();
    json!({ "model_info": models })
}

/// A message about an inner node or link, with where it is (`[at <JSON
/// pointer>] `, `docs/plugin-spec.md`), so an editor can mark it.
fn located(message: String) -> String {
    let pointer = if let Some(rest) = message.strip_prefix("links[") {
        rest.split_once(']')
            .filter(|(i, _)| i.parse::<usize>().is_ok())
            .map(|(i, _)| format!("/graph/links/{i}"))
    } else {
        ["nodes.", "node "].iter().find_map(|prefix| {
            let id = message.strip_prefix(prefix)?.split_once(':')?.0;
            let plain = !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            plain.then(|| format!("/graph/nodes/{id}"))
        })
    };
    match pointer {
        Some(p) => format!("[at {p}] {message}"),
        None => message,
    }
}

fn answer(request: &Path, preflight_mode: bool) -> (Value, bool) {
    let parsed = std::fs::read_to_string(request)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
    let folder = request.parent().unwrap_or(Path::new("."));
    let request = match parsed {
        Ok(r) => r,
        Err(e) if preflight_mode => {
            return (
                json!({"problems": [Problem::permanent(format!("request: {e}"))]}),
                false,
            );
        }
        Err(e) => return (json!({"errors": [format!("request: {e}")]}), false),
    };
    let contract = request.get("contract").and_then(Value::as_u64);
    let args = request.get("launch_args").cloned().unwrap_or(Value::Null);
    if preflight_mode {
        if contract != Some(2) {
            let p = Problem::permanent("this pre-flight check speaks contract 2");
            return (json!({"problems": [p]}), false);
        }
        let (problems, warnings) = preflight(&args, folder);
        let ok = problems.is_empty();
        let problems: Vec<Problem> = problems
            .into_iter()
            .map(|p| Problem {
                message: located(p.message),
                retryable: p.retryable,
            })
            .collect();
        let warnings: Vec<String> = warnings.into_iter().map(located).collect();
        return (json!({"problems": problems, "warnings": warnings}), ok);
    }
    if request.get("mode").and_then(Value::as_str) == Some("describe") {
        if contract != Some(3) {
            return (
                json!({"errors": ["this describe mode speaks contract 3"]}),
                false,
            );
        }
        return (json!({"values": describe(&args, folder)}), true);
    }
    if contract != Some(1) {
        return (
            json!({"errors": ["this configurator speaks contract 1"]}),
            false,
        );
    }
    match configure(&args, folder) {
        Ok((values, warnings)) => {
            let warnings: Vec<String> = warnings.into_iter().map(located).collect();
            (json!({"values": values, "warnings": warnings}), true)
        }
        Err(errors) => {
            let errors: Vec<String> = errors.into_iter().map(located).collect();
            (json!({"errors": errors}), false)
        }
    }
}

/// The configurator program: `[--mode preflight|describe] --request <file> --response <file>`.
pub fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let flag = |name: &str| {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let (Some(request), Some(response)) = (flag("--request"), flag("--response")) else {
        eprintln!(
            "usage: flwrs-cv-source-config [--mode preflight|describe] --request <file> --response <file>"
        );
        return ExitCode::from(2);
    };
    let (text, ok) = answer(
        Path::new(&request),
        flag("--mode").as_deref() == Some("preflight"),
    );
    if let Err(e) = std::fs::write(&response, text.to_string()) {
        eprintln!("{response}: {e}");
        return ExitCode::FAILURE;
    }
    // Ends a model load still running past its budget.
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensor::Output;
    use crate::testing;

    /// A node folder with a fixed detector (rows of 6) in `config/`.
    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        let rows = Output::new(vec![1, 1, 6], vec![0.0; 6]).unwrap();
        std::fs::write(
            dir.path().join("config/det.onnx"),
            testing::constant_outputs(&[1, 3, 32, 64], &[("rows", &rows)], &[], &[]),
        )
        .unwrap();
        dir
    }

    fn args(output: &str, width: u32, extra: Value) -> Value {
        let mut nodes = json!({
            "camera": { "type": "source" },
            "det": {
                "type": "model", "model": "det.onnx",
                "pre": { "width": width, "height": 32, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw" },
                "decode": { "type": "detections_e2e", "output": output }
            }
        });
        let mut links = vec![json!({ "from": "camera.frame", "to": "det.frame" })];
        if let Some(emit) = extra.get("emit") {
            nodes["out"] = emit.clone();
            links.push(json!({ "from": "det.objects", "to": "out.objects" }));
        }
        json!({ "graph": { "nodes": nodes, "links": links }, "files": ["det.onnx"] })
    }

    fn field_keys(schema: &Value) -> Vec<&str> {
        schema["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["key"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn messages_about_the_graph_say_where() {
        assert_eq!(
            located("nodes.det: input frame isn't linked".into()),
            "[at /graph/nodes/det] nodes.det: input frame isn't linked"
        );
        assert_eq!(
            located("node crop_2: pre is 64x64".into()),
            "[at /graph/nodes/crop_2] node crop_2: pre is 64x64"
        );
        assert_eq!(
            located("links[3] (a.x → b.y): no".into()),
            "[at /graph/links/3] links[3] (a.x → b.y): no"
        );
        for other in [
            "graph: missing",
            "input: bad",
            "nodes.Bad Id: x",
            "links[x]: y",
        ] {
            assert_eq!(located(other.into()), other);
        }
    }

    #[test]
    fn describing_reads_files_whatever_the_graph_is() {
        let dir = folder();
        let names: Vec<String> = std::fs::read_dir(dir.path().join("config"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        std::fs::write(dir.path().join("config/labels.txt"), "a\nb\n").unwrap();
        let mut files = names.clone();
        files.push("labels.txt".into());
        files.push("missing.onnx".into());
        let info = describe(&json!({ "files": files }), dir.path());
        let models = info["model_info"].as_object().unwrap();
        assert_eq!(models.len(), names.len(), "{models:?}");
        for name in &names {
            assert!(models[name]["outputs"].is_array());
        }
    }

    #[test]
    fn the_out_schema_follows_the_graph() {
        let dir = folder();
        let emit = json!({ "emit": { "type": "emit", "mode": "frames", "keypoints": false } });
        let (values, warnings) = configure(&args("rows", 64, emit), dir.path()).unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        let schema = &values["out_schema"];
        let keys = field_keys(schema);
        assert!(keys.contains(&"objects"), "{keys:?}");
        assert!(!keys.contains(&"rule"), "no rules: {keys:?}");
        assert!(!keys.contains(&"data"), "no Lua: {keys:?}");
        assert!(!schema.to_string().contains("\"keypoints\""));
        let parsed = flwrs_plugin::schema::rules::parse_json(&schema.to_string()).unwrap();
        assert!(flwrs_plugin::schema::rules::check_well_formed(&parsed).is_empty());
        assert_eq!(
            values["model_info"]["det.onnx"]["outputs"][0]["name"],
            "rows"
        );
    }

    #[test]
    fn a_save_warns_about_what_wont_fit() {
        let dir = folder();
        let (_, warnings) = configure(&args("nope", 48, json!({})), dir.path()).unwrap();
        for want in ["no output \"nope\"", "pre is 48x32", "sends nothing"] {
            assert!(
                warnings.iter().any(|w| w.contains(want)),
                "{want}: {warnings:?}"
            );
        }
    }

    #[test]
    fn a_missing_file_warns_at_save_and_stops_a_start() {
        let dir = tempfile::tempdir().unwrap();
        let settings = args("rows", 64, json!({}));
        let (_, warnings) = configure(&settings, dir.path()).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("det.onnx")),
            "{warnings:?}"
        );
        let (problems, _) = preflight(&settings, dir.path());
        assert!(
            problems
                .iter()
                .any(|p| !p.retryable && p.message.contains("det.onnx"))
        );
    }

    #[test]
    fn a_broken_graph_is_refused() {
        let dir = folder();
        let errors = configure(&json!({ "graph": { "nodes": {} } }), dir.path()).unwrap_err();
        assert!(!errors.is_empty());
    }

    #[test]
    fn a_camera_that_isnt_there_is_retryable() {
        let dir = folder();
        let mut settings = args("rows", 64, json!({}));
        settings["input"] = json!({ "type": "camera", "device": "no-such-camera-here" });
        let (problems, _) = preflight(&settings, dir.path());
        if cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            assert!(
                problems
                    .iter()
                    .any(|p| p.retryable && p.message.contains("camera")),
                "{problems:?}"
            );
        }
    }

    #[test]
    fn a_healthy_node_passes() {
        let dir = folder();
        let (problems, warnings) = preflight(&args("rows", 64, json!({})), dir.path());
        assert_eq!(problems, vec![], "{warnings:?}");
    }
}
