mod config;
mod conn;
mod plugin;
mod tls;
mod topic;

use clap::Parser;
use config::Args;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use plugin::MqttSource;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let settings = match args.settings() {
        Ok(settings) => settings,
        Err(problems) => {
            for problem in problems {
                eprintln!("{problem}");
            }
            return ExitCode::from(2);
        }
    };
    if settings.insecure_skip_verify {
        eprintln!(
            "WARNING: insecure_skip_verify is on: any broker certificate is accepted (development only)"
        );
    }
    let config = SourceRunnerConfig {
        plugin_id: args.bootstrap.id.clone(),
        log_level: args.bootstrap.log_level(),
        hub_connection: args.bootstrap.connection(),
    };
    let mut runner = match SourceRunner::initialize(MqttSource::new(settings), config).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize the source runner: {err}");
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
    use crate::config::{AckMode, Args, PASSWORD_ENV, RetainHandling};
    use clap::CommandFactory;
    use flwrs_plugin::cli::Bootstrap;
    use flwrs_plugin::package::{LaunchSpec, Manifest, Via};
    use std::collections::BTreeSet;

    fn json(path: &str) -> serde_json::Value {
        let text =
            std::fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn manifest_matches_the_crate() {
        let manifest = json("manifest.json");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["type"], "SOURCE");
        assert_eq!(
            manifest["executable"],
            format!("bin/{}", env!("CARGO_PKG_NAME"))
        );
    }

    #[test]
    fn launch_args_schema_matches_the_cli() {
        let manifest: Manifest = serde_json::from_value(json("manifest.json")).unwrap();
        let schema = json("schema/launch_args.json");
        let (spec, report) = LaunchSpec::new(&manifest, Some(&schema));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let flags: BTreeSet<String> = spec
            .fields()
            .filter(|f| f.via != Via::Env)
            .map(|f| f.target.clone())
            .collect();
        let from_cli: BTreeSet<String> = Args::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .filter(|l| !Bootstrap::NAMES.contains(&l.as_str()))
            .collect();
        assert_eq!(flags, from_cli);
        let env: Vec<(&str, bool)> = spec
            .fields()
            .filter(|f| f.via == Via::Env)
            .map(|f| (f.target.as_str(), f.secret))
            .collect();
        assert_eq!(env, [(PASSWORD_ENV, true)]);
        let key = spec.fields().find(|f| f.name == "tls_key").unwrap();
        assert!(key.secret && key.upload);
    }

    /// Settings valid by the schema run: planned like the server, then parsed.
    #[test]
    fn launch_args_schema_agrees_with_the_cli() {
        use clap::Parser;
        use flwrs_plugin::package::ConfigContent;
        let manifest: Manifest = serde_json::from_value(json("manifest.json")).unwrap();
        let schema = json("schema/launch_args.json");
        let validator = jsonschema::validator_for(&schema).unwrap();
        let args = serde_json::json!({
            "broker_url": "mqtts://broker",
            "subscriptions": [
                {"filter": "sensors/#"},
                {"filter": "$share/flwrs/cmd/+", "qos": 2, "no_local": true, "retain_handling": "new_only"}
            ],
            "ack_mode": "on_emit",
            "clean_start": true,
            "session_expiry_s": 60,
            "username": "u",
            "password": "p",
            "max_inflight": 8,
            "max_qos0_buffered": 10,
            "last_will": {"topic": "status/x", "payload": "gone"}
        });
        assert!(
            validator.is_valid(&args),
            "{:?}",
            validator
                .iter_errors(&args)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        let (spec, _) = LaunchSpec::new(&manifest, Some(&schema));
        let dir = tempfile::tempdir().unwrap();
        let plan = spec.plan(&args, dir.path()).unwrap();
        for file in &plan.files {
            let ConfigContent::Data(bytes) = &file.content else {
                panic!()
            };
            std::fs::write(dir.path().join(&file.name), bytes).unwrap();
        }
        let password = plan
            .env
            .iter()
            .find(|(k, _)| k == PASSWORD_ENV)
            .map(|(_, v)| v.clone());
        let mut argv = vec![
            "s".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
        ];
        argv.extend(plan.argv);
        let s = Args::try_parse_from(argv)
            .unwrap()
            .settings_with(password)
            .unwrap();
        assert_eq!(s.subscriptions.len(), 2);
        assert_eq!(s.subscriptions[1].retain_handling, RetainHandling::NewOnly);
        assert_eq!(
            (s.ack_mode, s.clean_start, s.session_expiry_s),
            (AckMode::OnEmit, true, Some(60))
        );
        assert_eq!(s.credentials, Some(("u".into(), "p".into())));
        for bad in [
            serde_json::json!({"broker_url": "mqtt://b"}),
            serde_json::json!({"broker_url": "mqtt://b", "subscriptions": []}),
            serde_json::json!({"broker_url": "mqtt://b", "subscriptions": [{"filter": "a", "qos": 3}]}),
            serde_json::json!({"broker_url": "mqtt://b", "subscriptions": [{"qos": 1}]}),
            serde_json::json!({"broker_url": "mqtt://b", "subscriptions": [{"filter": "a"}], "ack_mode": "later"}),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
    }
}
