mod config;
mod conn;
mod plugin;
mod tls;
mod topic;

use clap::Parser;
use config::Args;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use plugin::MqttSink;
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
    // One event per MQTT in-flight slot; the hub keeps a window of twice that.
    let inflight = usize::from(settings.max_inflight);
    let config = SinkRunnerConfig::new(
        args.bootstrap.id.clone(),
        args.bootstrap.log_level(),
        args.bootstrap.connection(),
    )
    .with_max_unacked(u32::from(settings.max_inflight).saturating_mul(2))
    .with_max_concurrent(inflight);
    let mut runner = match SinkRunner::initialize(MqttSink::new(settings), config).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize the sink runner: {err}");
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
    use crate::config::{Args, PASSWORD_ENV};
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
        assert_eq!(manifest["type"], "SINK");
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
            "broker_url": "mqtt://broker:1883",
            "protocol_version": "3.1.1",
            "client_id": "c1",
            "keep_session": true,
            "keepalive_s": 60,
            "username": "u",
            "password": "p",
            "max_inflight": 4,
            "max_packet_bytes": 4096,
            "last_will": {"topic": "status/c1", "payload": "offline", "qos": 1, "retain": true},
            "default_topic": "out",
            "default_qos": 0,
            "allowed_topics": ["dev/+/out"]
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
        assert_eq!(password.as_deref(), Some("p"));
        let mut argv = vec![
            "s".to_string(),
            "--id".into(),
            "n".into(),
            "--port".into(),
            "1".into(),
        ];
        argv.extend(plan.argv);
        let settings = Args::try_parse_from(argv)
            .unwrap()
            .settings_with(password)
            .unwrap();
        assert_eq!(settings.max_inflight, 4);
        assert!(!settings.clean_start);
        assert_eq!(settings.allowed_topics, ["dev/+/out"]);
        assert_eq!(settings.last_will.unwrap().payload, "offline");
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"broker_url": "mqtt://b", "default_qos": 3}),
            serde_json::json!({"broker_url": "mqtt://b", "protocol_version": "4"}),
            serde_json::json!({"broker_url": "mqtt://b", "max_inflight": 0}),
            serde_json::json!({"broker_url": "mqtt://b", "last_will": {"payload": "x"}}),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
    }
}
