mod plugin;
mod settings;

use crate::settings::Args;
use clap::Parser;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use std::process::ExitCode;

fn main() -> ExitCode {
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
    let source = plugin::VisionSource {
        settings,
        plan: std::sync::Mutex::new(plan),
        warnings,
        sink: None,
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

    /// The schema refuses what the plugin would.
    #[test]
    fn the_schema_refuses_bad_graphs() {
        let schema = jsonschema::validator_for(&json("schema/launch_args.json")).unwrap();
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"graph": {"nodes": {"Camera": {"kind": "source"}}}}),
            serde_json::json!({"graph": {"nodes": {"c": {"kind": "camera"}}}}),
            serde_json::json!({"graph": {"nodes": {"m": {"kind": "model", "model": "a.onnx",
                "decode": {"type": "classify", "output": "o", "softmax": true, "extra": 1}}}}}),
            serde_json::json!({"graph": {"nodes": {"c": {"kind": "source"}}, "links": [{"from": "c", "to": "d.frame"}]}}),
        ] {
            assert!(!schema.is_valid(&bad), "{bad}");
        }
    }
}
