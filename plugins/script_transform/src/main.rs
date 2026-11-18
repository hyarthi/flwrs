use clap::Parser;
use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};
use flwrs_script_transform::config::Args;
use flwrs_script_transform::engine::Engine;
use flwrs_script_transform::plugin::ScriptTransform;
use flwrs_script_transform::store::Store;
use std::path::Path;
use std::process::ExitCode;

/// The state file, in the working directory: the node's own folder.
const STATE_FILE: &str = "state.db";

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
    let engine = Store::open(Path::new(STATE_FILE), settings.state.clone()).and_then(|store| {
        Engine::new(
            &settings.script,
            settings.output.clone(),
            settings.limits,
            store,
        )
    });
    let engine = match engine {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let transform = ScriptTransform {
        engine,
        input: settings.input,
        output: settings.output,
        on_error: settings.on_error,
    };
    let config = TransformRunnerConfig::new(
        args.bootstrap.id.clone(),
        args.bootstrap.log_level(),
        args.bootstrap.connection(),
    );
    let mut runner = match TransformRunner::initialize(transform, config).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize the transform runner: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runner.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("Plugin stopped with an error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Keeps the package files (`manifest.json`, `schema/launch_args.json`) in
/// sync with the code.
#[cfg(test)]
mod package_tests {
    use clap::CommandFactory;
    use flwrs_plugin::cli::Bootstrap;
    use flwrs_script_transform::config::Args;
    use std::collections::BTreeSet;

    fn read(path: &str) -> String {
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + path)
            .unwrap()
    }

    fn json(path: &str) -> serde_json::Value {
        serde_json::from_str(&read(path)).unwrap()
    }

    #[test]
    fn manifest_matches_the_crate() {
        let manifest = json("manifest.json");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["type"], "TRANSFORM");
        assert_eq!(
            manifest["executable"],
            format!("bin/{}", env!("CARGO_PKG_NAME"))
        );
    }

    /// The flags the server passes (by the schema's delivery rules) are the
    /// CLI's, and the package is valid manifest v2.
    #[test]
    fn launch_args_schema_matches_the_cli() {
        use flwrs_plugin::package::{LaunchSpec, Manifest};
        let manifest: Manifest = serde_json::from_value(json("manifest.json")).unwrap();
        let schema = json("schema/launch_args.json");
        let (spec, report) = LaunchSpec::new(&manifest, Some(&schema));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let from_schema: BTreeSet<String> = spec.fields().map(|f| f.target.clone()).collect();
        let from_cli: BTreeSet<String> = Args::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .filter(|l| !Bootstrap::NAMES.contains(&l.as_str()))
            .collect();
        assert_eq!(from_schema, from_cli);
        let on_error: Vec<&str> = schema["properties"]["on_error"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(on_error, ["nack", "nack_retry", "drop"]);
    }

    /// Settings valid by the schema run: planned like the server, then parsed.
    #[test]
    fn launch_args_schema_agrees_with_the_cli() {
        use clap::Parser;
        use flwrs_plugin::package::{ConfigContent, LaunchSpec, Manifest};
        let manifest: Manifest = serde_json::from_value(json("manifest.json")).unwrap();
        let schema = json("schema/launch_args.json");
        let validator = jsonschema::validator_for(&schema).unwrap();
        let args = serde_json::json!({
            "schemas": {"in": {"fields": [{"key": "x", "type": "I64"}]},
                        "out": {"fields": [{"key": "y", "type": "I64"}]}},
            "script": "return { y = payload.x * 2 }",
            "limits": {"timeout_ms": 200},
            "state": {"max_keys": 10, "sync": "full"},
            "on_error": "drop"
        });
        assert!(validator.is_valid(&args));
        let (spec, _) = LaunchSpec::new(&manifest, Some(&schema));
        let dir = tempfile::tempdir().unwrap();
        let plan = spec.plan(&args, dir.path()).unwrap();
        for file in &plan.files {
            let ConfigContent::Data(bytes) = &file.content else {
                panic!()
            };
            std::fs::write(dir.path().join(&file.name), bytes).unwrap();
        }
        let mut argv = vec![
            "script".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
        ];
        argv.extend(plan.argv);
        let settings = Args::try_parse_from(argv).unwrap().settings().unwrap();
        assert_eq!(settings.limits.timeout.as_millis(), 200);
        assert_eq!(settings.state.max_keys, 10);
        for bad in [
            serde_json::json!({"script": "x"}),
            serde_json::json!({"schemas": {"in": {}, "out": {}}, "script": "x", "on_error": "ignore"}),
            serde_json::json!({"schemas": {"in": {}, "out": {}}, "script": "x", "limits": {"timeout_ms": 0}}),
            serde_json::json!({"schemas": {"in": {}, "out": {}}, "script": "x", "state": {"sync": "sometimes"}}),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
    }
}
