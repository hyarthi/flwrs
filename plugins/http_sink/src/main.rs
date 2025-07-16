mod plugin;
mod schema;

use crate::plugin::PluginSettings;
use clap::Parser;
use flwrs_plugin::plugin::core::ConnectionConfig;
use flwrs_plugin::schema::common::log_level::Enum as LogLevel;
use flwrs_plugin::sink::runner::{SinkRunner, SinkRunnerConfig};
use std::time::Duration;

/// flwrs sink plugin to send an HTTP request
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub(crate) struct Args {
    /// Plugin ID
    #[arg(short, long, required = true)]
    id: String,

    /// Hostname/IP of the hub. Can be empty for localhost
    #[arg(short, long, required = true, default_value = "")]
    host: String,

    /// Port of the hub
    #[arg(short, long, required = true)]
    port: u16,

    /// Log level
    #[arg(short, long, required = true, default_value = "info")]
    log_level: String,

    /// HTTP client timeout (seconds)
    #[arg(short, long, required = false, default_value = "60")]
    http_timeout_seconds: usize,

    /// HTTP client read timeout (seconds)
    #[arg(short, long, required = false, default_value = "30")]
    http_read_timeout_seconds: usize,

    /// HTTP client connect timeout (seconds)
    #[arg(short, long, required = false, default_value = "30")]
    http_connect_timeout_seconds: usize,

    /// HTTP client enable verbose logging
    #[arg(short, long, required = false, default_value = "false")]
    http_verbose_logging: bool,
}

#[tokio::main]
async fn main() {
    println!("Starting plugin...");
    let args = Args::parse();

    let settings = PluginSettings {
        connect_timeout: Duration::from_secs(args.http_connect_timeout_seconds as u64),
        read_timeout: Duration::from_secs(args.http_read_timeout_seconds as u64),
        timeout: Duration::from_secs(args.http_timeout_seconds as u64),
        verbose_logging: args.http_verbose_logging,
    };
    let plugin = match plugin::Plugin::new(args.id.as_str(), settings) {
        Ok(plugin) => plugin,
        Err(err) => {
            eprintln!("Failed to initialize plugin: {}", err);
            return;
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
    let cfg = SinkRunnerConfig {
        plugin_id: args.id,
        log_level,
        hub_connection: ConnectionConfig {
            host: args.host,
            port: args.port,
        },
    };
    let mut runner = match SinkRunner::initialize(plugin, cfg).await {
        Ok(runner) => runner,
        Err(err) => {
            eprintln!("Failed to initialize sink runner: {}", err);
            return;
        }
    };

    match runner.run().await {
        Ok(_) => (),
        Err(err) => {
            log::error!("Plugin crashed: {}", err);
        }
    };

    log::debug!("Plugin stopped");
}
