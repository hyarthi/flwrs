use std::fmt::{Display, Formatter};
use crate::config;
use lazy_static::lazy_static;
use std::sync::Arc;
use flexi_logger::FlexiLoggerError;
use serde::Deserialize;
use thiserror::Error;

mod loglog;

#[derive(Deserialize, Copy, Clone, Eq, PartialEq)]
pub enum FormatType {
    PLAIN,
}

#[derive(Deserialize, Copy, Clone, Eq, PartialEq)]
pub enum SinkType {
    CONSOLE,
    FILE,
    SYSLOG,
}

#[derive(Deserialize, Copy, Clone, Debug, Eq, PartialEq)]
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

lazy_static! {
    static ref ordered_log_levels: Vec<LogLevel> = vec![
        LogLevel::ERROR,
        LogLevel::WARN,
        LogLevel::INFO,
        LogLevel::DEBUG,
        LogLevel::TRACE,
    ];
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

#[derive(Deserialize)]
pub struct LoggerConfig {
    #[serde(default = "default_id")]
    id: String,
    format: FormatType,
    level: LogLevel,
    sinks: Vec<SinkConfig>,
}

fn default_id() -> String {
    "actor".to_string()
}

#[derive(Deserialize)]
pub struct SinkConfig {
    sink_type: SinkType,
    file_directory: Option<String>,
    file_max_size_bytes: Option<u64>,
    file_max_log_history: Option<u32>,
}

#[derive(Error, Debug)]
pub enum LogError {
    #[error("failed to read logging config struct")]
    ConfigReadError,
    #[error("log level not found: {0}")]
    LogLevelNotFoundError(LogLevel),
    #[error("syslog error: {0}")]
    SyslogError(#[from] #[source] syslog::Error),
    #[error(transparent)]
    FlexiLoggerError(#[from] FlexiLoggerError)
}

lazy_static! {
    static ref DEFAULT_LOGGER: Arc<dyn log::Log> = Arc::new(loglog::LogLogger::default());
}

pub fn default_logger() -> &'static dyn log::Log {
    DEFAULT_LOGGER.as_ref()
}

lazy_static! {
    static ref MAIN_LOGGER: Arc<dyn log::Log> = build_main_logger();
}

fn build_main_logger() -> Arc<dyn log::Log> {
    let main_config = config::main_config();

    let mut config = config::read_struct::<LoggerConfig>(main_config, &["logging".to_string()])
        .ok_or(LogError::ConfigReadError).unwrap();
    config.id = "actor".to_string();
    Arc::new(loglog::LogLogger::new(config).unwrap())
}

pub fn main_logger() -> &'static dyn log::Log {
    MAIN_LOGGER.as_ref()
}
