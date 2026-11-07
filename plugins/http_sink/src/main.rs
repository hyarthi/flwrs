mod plugin;
mod schema;

use crate::plugin::PluginSettings;
use clap::Parser;
use flwrs_plugin::plugin::core::ConnectionConfig;
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use std::process::ExitCode;
use std::time::Duration;

/// flwrs sink plugin to send an HTTP request
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub(crate) struct Args {
    /// Plugin ID
    #[arg(short, long, required = true)]
    id: String,

    /// Hostname/IP of the hub
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port of the hub
    #[arg(short, long)]
    port: u16,

    /// Connection token for the hub. Defaults to the FLWRS_TOKEN environment
    /// variable, which is preferred: command-line arguments are visible in `ps`.
    #[arg(long)]
    token: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short, long, default_value = "info")]
    log_level: String,

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
    let plugin = match plugin::Plugin::new(args.id.as_str(), settings) {
        Ok(plugin) => plugin,
        Err(err) => {
            eprintln!("Failed to initialize plugin: {}", err);
            return ExitCode::FAILURE;
        }
    };
    let log_level = match args.log_level.as_str() {
        "trace" => LogLevel::Trace,
        "debug" => LogLevel::Debug,
        "info" => LogLevel::Info,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };
    let concurrent = args.max_concurrent_requests;
    let cfg = SinkRunnerConfig::new(
        args.id,
        log_level,
        ConnectionConfig::new(args.host, args.port).with_token(args.token),
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
        const BOOTSTRAP: [&str; 5] = ["id", "host", "port", "log-level", "token"];
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
            .filter(|l| !BOOTSTRAP.contains(&l.as_str()))
            .collect();
        assert_eq!(
            from_schema, from_cli,
            "every non-bootstrap CLI flag must be in the schema and vice versa"
        );
    }
}
