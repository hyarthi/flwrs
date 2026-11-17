mod config;
mod conn;
mod json_path;
mod plugin;
mod tls;

use clap::Parser;
use config::Args;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use plugin::WsSink;
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
            "WARNING: insecure_skip_verify is on: any server certificate is accepted (development only)"
        );
    }
    let concurrent = settings.max_concurrent;
    let config = SinkRunnerConfig::new(
        args.bootstrap.id.clone(),
        args.bootstrap.log_level(),
        args.bootstrap.connection(),
    )
    .with_max_unacked(concurrent.saturating_mul(2).max(16))
    .with_max_concurrent(concurrent as usize);
    let mut runner = match SinkRunner::initialize(WsSink::new(settings), config).await {
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
    use crate::config::{AUTHORIZATION_ENV, Args};
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
        assert_eq!(env, [(AUTHORIZATION_ENV, true)]);
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
            "url": "wss://api.example.com/stream",
            "allowed_urls": ["wss://api.example.com/"],
            "headers": {"x-tenant": "t1"},
            "authorization": "Bearer abc",
            "subprotocols": ["v1.json"],
            "origin": "https://flwrs.local",
            "lazy_connect": true,
            "ping_interval_s": 5,
            "max_connections": 4,
            "message_type": "binary",
            "ack": "reply",
            "request_id_path": "meta.id",
            "reply_id_path": "id",
            "reply_error_path": "error",
            "reply_timeout_ms": 500,
            "max_concurrent": 8
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
        let authorization = plan
            .env
            .iter()
            .find(|(k, _)| k == AUTHORIZATION_ENV)
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
            .settings_with(authorization)
            .unwrap();
        assert_eq!(s.headers.len(), 3);
        assert_eq!(s.subprotocols, ["v1.json"]);
        assert!(s.lazy_connect && s.reply.is_some());
        assert_eq!(s.max_concurrent, 8);
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"url": "http://x"}),
            serde_json::json!({"url": "ws://x", "ack": "maybe"}),
            serde_json::json!({"url": "ws://x", "headers": {"a": 1}}),
            serde_json::json!({"url": "ws://x", "max_connections": 0}),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
    }
}
