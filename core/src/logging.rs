use crate::config;
use flexi_logger::FlexiLoggerError;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::sync::LazyLock;
use thiserror::Error;

mod loglog;

/// The log line format.
#[derive(Deserialize, Serialize, Copy, Clone, Debug, Eq, PartialEq, utoipa::ToSchema)]
pub enum FormatType {
    PLAIN,
}

/// Where log records go.
#[derive(Deserialize, Serialize, Copy, Clone, Debug, Eq, PartialEq, utoipa::ToSchema)]
pub enum SinkType {
    CONSOLE,
    FILE,
    SYSLOG,
}

/// The lowest level logged.
#[derive(Deserialize, Serialize, Copy, Clone, Debug, Eq, PartialEq, utoipa::ToSchema)]
pub enum LogLevel {
    TRACE,
    DEBUG,
    INFO,
    WARN,
    ERROR,
}

impl Display for LogLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.stringify())
    }
}

impl LogLevel {
    fn stringify(&self) -> String {
        match self {
            LogLevel::TRACE => "trace".to_string(),
            LogLevel::DEBUG => "debug".to_string(),
            LogLevel::INFO => "info".to_string(),
            LogLevel::WARN => "warn".to_string(),
            LogLevel::ERROR => "error".to_string(),
        }
    }
}

#[derive(Deserialize, Copy, Clone, Eq, PartialEq)]
pub enum FieldType {
    String,
    Int,
    Bool,
    Float,
    DateTime,
    Binary,
}

/// `[logging]`: the server's own log. Missing fields fall back to the
/// defaults (a console logger at INFO).
#[derive(Deserialize, Serialize, Clone, Debug, utoipa::ToSchema)]
#[serde(default)]
pub struct LoggerConfig {
    /// Used as the syslog process name and the log file prefix.
    id: String,
    /// The line format (only `PLAIN` so far).
    #[allow(dead_code)] // only PLAIN exists so far
    format: FormatType,
    level: LogLevel,
    /// Where records go: one entry per sink (`[[logging.sinks]]`).
    sinks: Vec<SinkConfig>,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            id: default_id(),
            format: FormatType::PLAIN,
            level: LogLevel::INFO,
            sinks: vec![SinkConfig {
                sink_type: SinkType::CONSOLE,
                file_directory: None,
                file_max_size_bytes: None,
                file_max_log_history: None,
            }],
        }
    }
}

fn default_id() -> String {
    "flwrs".to_string()
}

/// A log sink (`[[logging.sinks]]`).
#[derive(Deserialize, Serialize, Clone, Debug, utoipa::ToSchema)]
pub struct SinkConfig {
    sink_type: SinkType,
    /// `FILE`: the folder for the rotating log files.
    file_directory: Option<String>,
    /// `FILE`: rotate after this many bytes.
    file_max_size_bytes: Option<u64>,
    /// `FILE`: how many rotated files to keep.
    file_max_log_history: Option<u32>,
}

#[derive(Error, Debug)]
pub enum LogError {
    #[error("failed to read logging config struct")]
    ConfigReadError,
    #[error("syslog error: {0}")]
    SyslogError(
        #[from]
        #[source]
        syslog::Error,
    ),
    #[error(transparent)]
    FlexiLoggerError(#[from] FlexiLoggerError),
}

static DEFAULT_LOGGER: LazyLock<loglog::LogLogger> = LazyLock::new(loglog::LogLogger::default);

/// Console logger at INFO level.
pub fn default_logger() -> &'static dyn log::Log {
    &*DEFAULT_LOGGER
}

static MAIN_LOGGER: LazyLock<loglog::LogLogger> = LazyLock::new(build_main_logger);

fn build_main_logger() -> loglog::LogLogger {
    let main_config = config::main_config();
    match config::read_struct::<LoggerConfig>(main_config, &["logging".to_string()]) {
        Some(config) => loglog::LogLogger::new(config).unwrap_or_else(|e| {
            eprintln!("Failed to build logger from [logging] config ({e}); using console logger");
            loglog::LogLogger::default()
        }),
        None => {
            eprintln!("No valid [logging] config; using console logger at INFO");
            loglog::LogLogger::default()
        }
    }
}

/// Logger built from the `[logging]` config section.
pub fn main_logger() -> &'static dyn log::Log {
    &*MAIN_LOGGER
}

/// Installs [`main_logger`] as the global `log` logger and sets the max level.
pub fn init_main_logger() -> Result<(), log::SetLoggerError> {
    let logger = &*MAIN_LOGGER;
    log::set_logger(logger)?;
    log::set_max_level(logger.level().to_level_filter());
    Ok(())
}
