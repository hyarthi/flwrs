mod plugin;
mod settings;

use crate::settings::Args;
use clap::Parser;
use flwrs_cv_source::emit::{self, Emitter, Outbox};
use flwrs_cv_source::live::Live;
use flwrs_plugin::plugin::live::LiveFeedPublisher;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

fn main() -> ExitCode {
    // The debug mode has no hub, so none of the bootstrap arguments.
    if std::env::args().nth(1).as_deref() == Some("debug") {
        return flwrs_cv_source::debug::main(std::env::args().skip(1));
    }
    let args = Args::parse();
    // `--threads` (optional, from the server) sizes the runtime.
    match flwrs_plugin::rt::runtime(args.bootstrap.threads) {
        Ok(runtime) => runtime.block_on(run(args)),
        Err(e) => {
            eprintln!("Failed to start the runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> ExitCode {
    let settings = match args.settings() {
        Ok(settings) => settings,
        Err(problems) => {
            for problem in problems {
                eprintln!("{problem}");
            }
            return ExitCode::from(2);
        }
    };
    // Every model loaded (compiled for its device, one warm-up run) before
    // connecting: a model that can't run is a settings problem.
    let threads = args.bootstrap.threads.unwrap_or(0);
    let compiled = {
        let (graph, checked, files, execution) = (
            settings.graph.clone(),
            settings.checked.clone(),
            settings.files.clone(),
            settings.execution.clone(),
        );
        tokio::task::spawn_blocking(move || {
            flwrs_cv_source::graph::exec::compile(&graph, &checked, &files, &execution, threads)
        })
        .await
    };
    let (plan, warnings) = match compiled {
        Ok(Ok(compiled)) => compiled,
        Ok(Err(problems)) => {
            for problem in problems {
                eprintln!("{problem}");
            }
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("Loading the models failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let emitter = Arc::new(Mutex::new(Emitter::new(emit::config_of(&settings.graph))));
    let outbox = Arc::new(Outbox::new(emit::MAX_QUEUED_EVENTS));
    // No capability offered, and no thread, unless the setting turns it on.
    let live = if settings.live.any() {
        let feed = LiveFeedPublisher::new();
        match Live::new(&settings.live, &settings.graph, feed.clone()) {
            Ok(live) => Some((feed, Arc::new(Mutex::new(live)))),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let source = plugin::VisionSource {
        settings,
        plan: Arc::new(Mutex::new(plan)),
        warnings,
        sink: None,
        outbox,
        emitter,
        live,
    };
    let config = SourceRunnerConfig {
        plugin_id: args.bootstrap.id.clone(),
        log_level: args.bootstrap.log_level(),
        hub_connection: args.bootstrap.connection(),
    };
    let mut runner = match SourceRunner::initialize(source, config).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize the source runner: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Non-zero when the run failed or the hub rejected the plugin.
    match runner.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("Plugin stopped with an error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Keeps the package files (`manifest.json`, `schema/*.json`, the example
/// graphs) in sync with the code.
#[cfg(test)]
mod package_tests {
    use super::Args;
    use clap::CommandFactory;
    use flwrs_plugin::cli::Bootstrap;
    use flwrs_plugin::schema::rules;
    use std::collections::BTreeSet;

    fn path(p: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(p)
    }

    fn json(p: &str) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path(p)).unwrap()).unwrap()
    }

    #[test]
    fn manifest_matches_the_crate() {
        let manifest = json("manifest.json");
        assert_eq!(
            manifest["version"],
            env!("CARGO_PKG_VERSION"),
            "bump manifest.json with Cargo.toml"
        );
        assert_eq!(manifest["type"], "SOURCE");
        assert_eq!(
            manifest["executable"],
            format!("bin/{}", env!("CARGO_PKG_NAME"))
        );
    }

    #[test]
    fn the_default_out_schema_is_well_formed() {
        let declared = rules::parse_json(crate::settings::DEFAULT_OUT_SCHEMA).unwrap();
        assert_eq!(rules::check_well_formed(&declared), Vec::<String>::new());
    }

    #[test]
    fn launch_args_schema_matches_the_cli() {
        let schema = json("schema/launch_args.json");
        let from_schema: BTreeSet<String> = schema["properties"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| match v["x-flwrs-launch"]["alias"].as_str() {
                Some(alias) => alias.to_string(),
                None => k.replace('_', "-"),
            })
            .collect();
        let from_cli: BTreeSet<String> = Args::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .filter(|l| !Bootstrap::NAMES.contains(&l.as_str()))
            .collect();
        assert_eq!(
            from_schema, from_cli,
            "every non-bootstrap CLI flag must be in the schema and vice versa"
        );
    }

    /// The editor's catalog says what the plugin does: the same node types
    /// and settings, and for every example graph the same ports.
    #[test]
    fn the_graph_catalog_agrees_with_the_plugin() {
        use flwrs_cv_source::graph::{self, Ports};
        use flwrs_plugin::package::graph::{Catalog, Condition, Port};
        use serde_json::Value;

        let schema = json("schema/launch_args.json");
        let text = std::fs::read_to_string(path("schema/graph_catalog.json")).unwrap();
        let catalog = Catalog::parse(&text, &schema).unwrap();
        // Node types: those the schema's graph accepts.
        let in_schema: BTreeSet<String> = schema
            .pointer("/properties/graph/properties/nodes/additionalProperties/oneOf")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|r| {
                r["$ref"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let listed: BTreeSet<String> = catalog.node_types.keys().cloned().collect();
        assert_eq!(listed, in_schema);
        for (id, node) in &catalog.node_types {
            assert_eq!(node.settings, format!("#/$defs/{id}"));
        }
        // Every decoder has an output type.
        let decoders = schema["$defs"]["decoder"]["oneOf"].as_array().unwrap();
        let outputs = &catalog.node_types["model"].outputs[0];
        let map = &outputs.type_from.as_ref().unwrap().map;
        for decoder in decoders {
            let name = decoder["properties"]["type"]["const"].as_str().unwrap();
            assert!(map.contains_key(name), "decoder {name} has no output type");
        }

        fn holds(c: &Condition, node: &Value) -> bool {
            let value = node.pointer(&c.pointer).or(c.default.as_ref());
            if let Some(present) = c.present {
                return value.is_some() == present;
            }
            if let Some(v) = &c.equals {
                return value == Some(v);
            }
            value != c.not_equals.as_ref()
        }
        fn ports(list: &[Port], node: &Value) -> BTreeSet<(String, String)> {
            list.iter()
                .filter(|p| p.when.as_ref().is_none_or(|c| holds(c, node)))
                .map(|p| {
                    let t = p.port_type.clone().unwrap_or_else(|| {
                        let from = p.type_from.as_ref().unwrap();
                        from.map[node.pointer(&from.pointer).unwrap().as_str().unwrap()].clone()
                    });
                    (p.name.clone().unwrap_or_else(|| t.clone()), t)
                })
                .collect()
        }
        let actual = |list: &[(&str, graph::PortType)]| -> BTreeSet<(String, String)> {
            list.iter()
                .map(|(n, t)| (n.to_string(), t.name().to_string()))
                .collect()
        };
        let mut checked = 0;
        for entry in std::fs::read_dir(path("examples/graphs")).unwrap() {
            let file = entry.unwrap().path();
            let example: Value =
                serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let Ok(config) = serde_json::from_value::<graph::GraphConfig>(example["graph"].clone())
            else {
                panic!("{}", file.display());
            };
            for (id, node) in &config.nodes {
                let raw = &example["graph"]["nodes"][id];
                let kind = &catalog.node_types[raw["type"].as_str().unwrap()];
                let of = Ports::of(node);
                let at = format!("{} {id}", file.display());
                assert_eq!(ports(&kind.inputs, raw), actual(&of.inputs), "{at}: inputs");
                assert_eq!(
                    ports(&kind.outputs, raw),
                    actual(&of.outputs),
                    "{at}: outputs"
                );
                checked += 1;
            }
        }
        assert!(checked > 50, "only {checked} nodes checked");
    }

    /// Every example graph is valid by the launch-args schema and by the
    /// plugin's own check.
    #[test]
    fn the_example_graphs_pass_the_schema_and_the_check() {
        let schema = jsonschema::validator_for(&json("schema/launch_args.json")).unwrap();
        let dir = std::env::temp_dir().join(format!("cv-source-examples-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(path("examples/graphs")).unwrap() {
            let file = entry.unwrap().path();
            let example: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            // A build without Lua rightly refuses a graph with a Lua rule.
            let uses_lua = example["graph"]["nodes"]
                .as_object()
                .is_some_and(|nodes| nodes.values().any(|n| n["type"] == "lua"));
            if uses_lua && !cfg!(feature = "lua") {
                continue;
            }
            let (graph_config, _) = flwrs_cv_source::graph::parse(&example["graph"])
                .unwrap_or_else(|p| panic!("{}: {p:?}", file.display()));
            let files = flwrs_cv_source::graph::uploads(&graph_config);
            let args = serde_json::json!({ "graph": example["graph"], "files": files });
            let errors: Vec<String> = schema.iter_errors(&args).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{}: {errors:?}", file.display());
            let graph = dir.join(file.file_name().unwrap());
            std::fs::write(&graph, example["graph"].to_string()).unwrap();
            let mut argv = vec![
                "cv".to_string(),
                "--id".into(),
                "n".into(),
                "--port".into(),
                "1".into(),
                "--graph".into(),
                graph.to_str().unwrap().into(),
            ];
            for name in &files {
                argv.extend(["--file".into(), format!("config/{name}")]);
            }
            let parsed = <Args as clap::Parser>::try_parse_from(argv).unwrap();
            if let Err(problems) = parsed.settings() {
                panic!("{}: {problems:?}", file.display());
            }
            count += 1;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            count >= 22,
            "the spike's 21 pipelines and the gesture chain"
        );
    }

    /// The graph's files must be among the node's (`--file`, once per file).
    #[test]
    fn the_graph_s_files_must_be_given() {
        let dir = std::env::temp_dir().join(format!("cv-source-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let graph = dir.join("graph.json");
        let example = json("examples/graphs/yolov10n.json");
        std::fs::write(&graph, example["graph"].to_string()).unwrap();
        let settings = |files: &[&str]| {
            let mut argv = vec![
                "cv",
                "--id",
                "n",
                "--port",
                "1",
                "--graph",
                graph.to_str().unwrap(),
            ];
            for f in files {
                argv.extend(["--file", f]);
            }
            <Args as clap::Parser>::try_parse_from(argv)
                .unwrap()
                .settings()
        };
        let problems = settings(&[]).err().unwrap();
        assert!(
            problems[0].contains("isn't among the node's files"),
            "{problems:?}"
        );
        let problems = settings(&["config/yolov10n.onnx", "other/yolov10n.onnx"])
            .err()
            .unwrap();
        assert!(problems[0].contains("two files named"), "{problems:?}");
        let ok = settings(&["config/yolov10n.onnx", "config/unused.txt"])
            .ok()
            .unwrap();
        assert_eq!(
            ok.files["yolov10n.onnx"],
            std::path::Path::new("config/yolov10n.onnx")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `live` setting: the schema's defaults and limits are the code's.
    #[test]
    fn the_live_setting_matches_the_schema() {
        use flwrs_cv_source::live::{
            LiveConfig, MAX_PREVIEW_FPS, MAX_PREVIEW_WIDTH, MIN_PREVIEW_FPS, MIN_PREVIEW_WIDTH,
        };
        let schema = json("schema/launch_args.json");
        let live = &schema["properties"]["live"]["properties"];
        let preview = &live["preview"]["properties"];
        let defaults = LiveConfig::default();
        assert_eq!(live["stats"]["default"], defaults.stats);
        assert_eq!(preview["enabled"]["default"], defaults.preview.enabled);
        assert_eq!(
            preview["fps"]["default"],
            f64::from(defaults.preview.fps) as u32
        );
        assert_eq!(preview["width"]["default"], defaults.preview.width);
        assert_eq!(preview["fps"]["minimum"], f64::from(MIN_PREVIEW_FPS));
        assert_eq!(preview["fps"]["maximum"], MAX_PREVIEW_FPS as u32);
        assert_eq!(preview["width"]["minimum"], MIN_PREVIEW_WIDTH);
        assert_eq!(preview["width"]["maximum"], MAX_PREVIEW_WIDTH);
    }

    /// The `input` setting: what the schema and the plugin take and refuse.
    #[test]
    fn inputs_are_checked() {
        let schema = jsonschema::validator_for(&json("schema/launch_args.json")).unwrap();
        let graph = json("examples/graphs/yolov10n.json")["graph"].clone();
        let dir = tempfile::tempdir().unwrap();
        let graph_file = dir.path().join("graph.json");
        std::fs::write(&graph_file, graph.to_string()).unwrap();
        let model = "config/yolov10n.onnx";
        let check = |input: serde_json::Value| {
            let file = dir.path().join("input.json");
            std::fs::write(&file, input.to_string()).unwrap();
            let args = <Args as clap::Parser>::try_parse_from([
                "cv",
                "--id",
                "n",
                "--port",
                "1",
                "--graph",
                graph_file.to_str().unwrap(),
                "--file",
                model,
                "--input",
                file.to_str().unwrap(),
            ])
            .unwrap();
            let by_schema = schema.is_valid(&serde_json::json!({
                "graph": graph, "files": ["yolov10n.onnx"], "input": input
            }));
            (by_schema, args.settings().is_ok())
        };
        for good in [
            serde_json::json!({"type": "camera"}),
            serde_json::json!({"type": "camera", "device": "FaceTime", "width": 1280, "height": 720, "fps": 30, "format": "nv12", "backend": "native"}),
            serde_json::json!({"type": "images", "path": "frames", "fps": 5, "loop": true}),
            serde_json::json!({"type": "mjpeg", "path": "clip.mjpeg"}),
        ] {
            assert_eq!(check(good.clone()), (true, true), "{good}");
        }
        for bad in [
            serde_json::json!({"type": "rtsp"}),
            serde_json::json!({"type": "camera", "width": 0, "height": 720}),
            serde_json::json!({"type": "camera", "fps": 0}),
            serde_json::json!({"type": "camera", "format": "h264"}),
            serde_json::json!({"type": "images"}),
            serde_json::json!({"type": "mjpeg", "path": "a", "fps": 0}),
            serde_json::json!({"type": "camera", "extra": 1}),
        ] {
            assert_eq!(check(bad.clone()), (false, false), "{bad}");
        }
        // Beyond what a schema can say.
        assert_eq!(
            check(serde_json::json!({"type": "camera", "width": 640})),
            (true, false)
        );
    }

    /// The schema refuses what the plugin would.
    #[test]
    fn the_schema_refuses_bad_graphs() {
        let schema = jsonschema::validator_for(&json("schema/launch_args.json")).unwrap();
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"graph": {"nodes": {"Camera": {"type": "source"}}}}),
            serde_json::json!({"graph": {"nodes": {"c": {"type": "camera"}}}}),
            serde_json::json!({"graph": {"nodes": {"m": {"type": "model", "model": "a.onnx",
                "decode": {"type": "classify", "output": "o", "softmax": true, "extra": 1}}}}}),
            serde_json::json!({"graph": {"nodes": {"c": {"type": "source"}}, "links": [{"from": "c", "to": "d.frame"}]}}),
        ] {
            assert!(!schema.is_valid(&bad), "{bad}");
        }
    }
}
