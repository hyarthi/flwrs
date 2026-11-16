mod plugin;
mod schema;

use crate::plugin::PluginSettings;
use clap::Parser;
use flwrs_plugin::cli::Bootstrap;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use std::process::ExitCode;
use std::time::Duration;

/// flwrs sink plugin to send an HTTP request
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub(crate) struct Args {
    #[command(flatten)]
    bootstrap: Bootstrap,

    /// HTTP client timeout (seconds)
    #[arg(long, default_value = "60")]
    http_timeout_seconds: u64,

    /// HTTP client read timeout (seconds)
    #[arg(long, default_value = "30")]
    http_read_timeout_seconds: u64,

    /// HTTP client connect timeout (seconds)
    #[arg(long, default_value = "30")]
    http_connect_timeout_seconds: u64,

    /// HTTP client enable verbose logging
    #[arg(long)]
    http_verbose_logging: bool,

    /// How many requests may be in flight at once. Each event is
    /// acknowledged after its response, in whatever order responses arrive.
    #[arg(long, default_value = "16", value_parser = clap::value_parser!(u32).range(1..=1024))]
    max_concurrent_requests: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    println!("Starting plugin...");
    let args = Args::parse();

    let settings = PluginSettings {
        connect_timeout: Duration::from_secs(args.http_connect_timeout_seconds),
        read_timeout: Duration::from_secs(args.http_read_timeout_seconds),
        timeout: Duration::from_secs(args.http_timeout_seconds),
        verbose_logging: args.http_verbose_logging,
    };
    let plugin = match plugin::Plugin::new(args.bootstrap.id.as_str(), settings) {
        Ok(plugin) => plugin,
        Err(err) => {
            eprintln!("Failed to initialize plugin: {}", err);
            return ExitCode::FAILURE;
        }
    };
    let concurrent = args.max_concurrent_requests;
    let cfg = SinkRunnerConfig::new(
        args.bootstrap.id.clone(),
        args.bootstrap.log_level(),
        args.bootstrap.connection(),
    )
    // Room for the hub to send the next events while requests are running.
    .with_max_unacked(concurrent.saturating_mul(2))
    .with_max_concurrent(concurrent as usize);
    let mut runner = match SinkRunner::initialize(plugin, cfg).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize sink runner: {}", err);
            return ExitCode::FAILURE;
        }
    };

    // Non-zero when the run failed or the hub rejected the plugin, so a
    // supervisor or operator can tell.
    let code = match runner.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("Plugin stopped with an error: {}", err);
            ExitCode::FAILURE
        }
    };
    log::debug!("Plugin stopped");
    code
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
        assert_eq!(manifest["type"], "SINK");
        assert_eq!(
            manifest["executable"],
            format!("bin/{}", env!("CARGO_PKG_NAME"))
        );
    }

    #[test]
    fn in_schema_matches_the_code() {
        let declared = rules::parse_json(&read("schema/in.json")).unwrap();
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
    }
}
