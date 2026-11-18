mod config;
mod plugin;
mod schedule;
mod schema;
mod ticker;

use crate::config::Args;
use crate::ticker::{Clock, Ticker};
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
    let settings = match args.settings(jiff::Timestamp::now()) {
        Ok(settings) => settings,
        Err(problems) => {
            for problem in problems {
                eprintln!("{problem}");
            }
            return ExitCode::from(2);
        }
    };
    let clock = Clock::system(settings.monotonic);
    let source = plugin::TickerSource {
        ticker: Ticker::new(settings, clock),
        label: args.label,
        confirmed: args.confirmed,
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

/// Keeps the package files (`manifest.json`, `schema/*.json`) in sync with the code.
#[cfg(test)]
mod package_tests {
    use super::Args;
    use clap::CommandFactory;
    use flwrs_plugin::cli::Bootstrap;
    use flwrs_plugin::schema::rules;
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
    fn out_schema_matches_the_code() {
        let declared = rules::parse_json(&read("schema/out.json")).unwrap();
        assert!(rules::check_well_formed(&declared).is_empty());
        let code = crate::schema::build_schema().into();
        assert_eq!(rules::diff(&declared, &code), Vec::<String>::new());
    }

    #[test]
    fn launch_args_schema_matches_the_cli() {
        let schema = json("schema/launch_args.json");
        let from_schema: BTreeSet<String> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.replace('_', "-"))
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
        // The schema's enum is the CLI's.
        let missed: Vec<&str> = schema["properties"]["missed"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(missed, ["skip", "fire_once", "catch_up"]);
    }

    /// Launch args valid by the schema parse on the command line, and the
    /// schema refuses what the CLI would.
    #[test]
    fn launch_args_schema_agrees_with_the_cli() {
        use flwrs_plugin::package::{LaunchArgsStyle, launch_args_to_argv};
        let schema = jsonschema::validator_for(&json("schema/launch_args.json")).unwrap();
        let ok = serde_json::json!({
            "cron": "*/5 * * * * *", "timezone": "Europe/Berlin", "fire_on_start": true,
            "jitter": "100ms", "missed": "catch_up", "max_catch_up": 10, "label": "a",
            "start_time": "2026-01-01T00:00:00Z", "confirmed": true
        });
        assert!(schema.is_valid(&ok));
        let mut argv = vec![
            "ticker".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
        ];
        argv.extend(launch_args_to_argv(LaunchArgsStyle::Flags, &ok).unwrap());
        let args = <Args as clap::Parser>::try_parse_from(argv).unwrap();
        args.settings(jiff::Timestamp::now()).unwrap();
        for bad in [
            serde_json::json!({"interval": "1s", "cron": "* * * * *"}),
            serde_json::json!({"interval": "1s", "timezone": "UTC"}),
            serde_json::json!({"cron": "* * * * *", "align": true}),
            serde_json::json!({"max_catch_up": 0}),
            serde_json::json!({"missed": "never"}),
            serde_json::json!({"interval": "1 s"}),
            serde_json::json!({"color": "red"}),
        ] {
            assert!(!schema.is_valid(&bad), "{bad}");
        }
    }
}
