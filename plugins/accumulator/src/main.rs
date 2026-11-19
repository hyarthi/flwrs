use clap::Parser;
use flwrs_accumulator::config::Args;
use flwrs_accumulator::engine::{Accumulator, Code};
use flwrs_accumulator::plugin::AccumulatorTransform;
use flwrs_plugin::transform::runner::{TransformRunner, TransformRunnerConfig};
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
        Accumulator::new(
            Code {
                init: settings.init.as_deref(),
                step: &settings.step,
                finish: settings.finish.as_deref(),
            },
            settings.key.clone(),
            settings.window,
            settings.output.clone(),
            settings.late_close,
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
    let transform =
        AccumulatorTransform::new(engine, settings.input, settings.output, settings.on_error);
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

/// Keeps the package files in sync with the code.
#[cfg(test)]
mod package_tests {
    use clap::CommandFactory;
    use flwrs_accumulator::config::Args;
    use flwrs_plugin::cli::Bootstrap;
    use std::collections::BTreeSet;

    fn json(path: &str) -> serde_json::Value {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + path)
                .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn read(path: &str) -> String {
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + path)
            .unwrap()
    }

    /// The templates the editor offers (`x-flwrs-template`) fit the settings
    /// schema, and init and step run on the schemas' events as they are.
    #[test]
    fn the_templates_work_together() {
        use flwrs_accumulator::engine::{Accumulator, Code, LateClose};
        use flwrs_accumulator::window::Window;
        use flwrs_plugin::schema::rules::parse_json;
        use flwrs_script_transform::store::{StateSettings, Store};
        use flwrs_types::path::Path;
        use flwrs_types::value::Value;

        let schemas = json("templates/schemas.json");
        let (init, step) = (read("templates/init.lua"), read("templates/step.lua"));
        let args =
            serde_json::json!({"schemas": schemas, "init": init, "step": step, "key": "device"});
        let schema = json("schema/launch_args.json");
        for (field, file) in [
            ("schemas", "schemas.json"),
            ("init", "init.lua"),
            ("step", "step.lua"),
        ] {
            assert_eq!(
                schema["properties"][field]["x-flwrs-template"],
                format!("templates/{file}")
            );
        }
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&args)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");

        let output = parse_json(&schemas["out"].to_string()).unwrap();
        let mut acc = Accumulator::new(
            Code {
                init: Some(&init),
                step: &step,
                finish: None,
            },
            Some(Path::parse("device").unwrap()),
            Window::None { idle_ttl: None },
            output,
            LateClose::Run,
            Default::default(),
            Store::memory(StateSettings::default()).unwrap(),
        )
        .unwrap();
        let reading = |v: f64| {
            Value::Object(vec![
                ("device".into(), Value::String("d1".into())),
                ("value".into(), Value::F64(v)),
            ])
            .into_payload()
            .unwrap()
        };
        acc.process_at(&reading(2.0), None, 0).unwrap();
        let out = acc.process_at(&reading(4.0), None, 1).unwrap();
        let text = format!("{:?}", Value::from_payload(&out[0]));
        assert!(
            text.contains("\"d1\"") && text.contains("I64(2)") && text.contains("F64(3.0)"),
            "{text}"
        );
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
            "schemas": {"in": {"fields": [{"key": "d", "type": "STRING"}]},
                        "out": {"fields": [{"key": "n", "type": "I64"}]}},
            "key": "d",
            "init": "return 0",
            "step": "return state + 1",
            "finish": "return { n = state }",
            "window": {"type": "hopping", "size": "1m", "hop": "10s"},
            "late_close": "drop",
            "state": {"max_keys": 10}
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
            "acc".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
        ];
        argv.extend(plan.argv);
        let settings = Args::try_parse_from(argv).unwrap().settings().unwrap();
        assert_eq!(settings.state.max_keys, 10);
        for bad in [
            serde_json::json!({"step": "x"}),
            serde_json::json!({"schemas": {"in": {}, "out": {}}, "step": "x", "window": {"type": "sliding"}}),
            serde_json::json!({"schemas": {"in": {}, "out": {}}, "step": "x", "late_close": "maybe"}),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
    }
}
