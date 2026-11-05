use super::{FormatType, LogError, LogLevel, LoggerConfig, SinkConfig, SinkType};
use flexi_logger::{Age, Cleanup, Criterion, FileSpec, LoggerHandle, Naming};
use log::{Level, Metadata, Record};
use syslog::Facility;

/// `log::Log` that fans records out to every configured sink.
pub(crate) struct LogLogger {
    level: Level,
    sinks: Vec<Box<dyn log::Log>>,
    /// Kept alive so flexi_logger's writers (e.g. file rotation) keep working.
    _flexi_handles: Vec<LoggerHandle>,
}

impl log::Log for LogLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        for sink in &self.sinks {
            sink.log(record);
        }
    }

    fn flush(&self) {
        for sink in &self.sinks {
            sink.flush();
        }
    }
}

impl Default for LogLogger {
    fn default() -> Self {
        Self::new(LoggerConfig {
            id: super::default_id(),
            format: FormatType::PLAIN,
            level: LogLevel::INFO,
            sinks: vec![SinkConfig {
                sink_type: SinkType::CONSOLE,
                file_directory: None,
                file_max_size_bytes: None,
                file_max_log_history: None,
            }],
        })
        .unwrap_or_else(|e| {
            eprintln!("Failed to create default logger: {e}");
            LogLogger {
                level: Level::Info,
                sinks: vec![],
                _flexi_handles: vec![],
            }
        })
    }
}

impl LogLogger {
    pub fn new(config: LoggerConfig) -> Result<Self, LogError> {
        let level: Level = config.level.into();
        let level_spec = config.level.stringify();
        let mut sinks: Vec<Box<dyn log::Log>> = vec![];
        let mut handles = vec![];
        for sink_cfg in &config.sinks {
            match sink_cfg.sink_type {
                SinkType::CONSOLE => {
                    let (sink, handle) = flexi_logger::Logger::try_with_str(&level_spec)?
                        .log_to_stdout()
                        .format(flexi_logger::colored_with_thread)
                        .build()?;
                    sinks.push(sink);
                    handles.push(handle);
                }
                SinkType::FILE => {
                    let directory = sink_cfg
                        .file_directory
                        .clone()
                        .unwrap_or_else(|| "logs/".to_string());
                    let (sink, handle) = flexi_logger::Logger::try_with_str(&level_spec)?
                        .log_to_file(
                            FileSpec::default()
                                .basename(format!("{}_log", config.id))
                                .directory(directory),
                        )
                        .format(flexi_logger::detailed_format)
                        .rotate(
                            Criterion::AgeOrSize(
                                Age::Day,
                                sink_cfg.file_max_size_bytes.unwrap_or(5 * 1024 * 1024),
                            ),
                            Naming::TimestampsCustomFormat {
                                current_infix: Some("LATEST"),
                                format: "%Y-%m-%d_%H:%M:%S",
                            },
                            Cleanup::KeepLogFiles(
                                sink_cfg.file_max_log_history.unwrap_or(10) as usize
                            ),
                        )
                        .build()?;
                    sinks.push(sink);
                    handles.push(handle);
                }
                SinkType::SYSLOG => {
                    let logger = syslog::unix(syslog::Formatter3164 {
                        facility: Facility::LOG_USER,
                        hostname: None,
                        process: config.id.clone(),
                        pid: std::process::id(),
                    })?;
                    sinks.push(Box::new(syslog::BasicLogger::new(logger)));
                }
            };
        }

        Ok(LogLogger {
            level,
            sinks,
            _flexi_handles: handles,
        })
    }

    pub fn level(&self) -> Level {
        self.level
    }
}

impl From<Level> for LogLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::Error => LogLevel::ERROR,
            Level::Warn => LogLevel::WARN,
            Level::Info => LogLevel::INFO,
            Level::Debug => LogLevel::DEBUG,
            Level::Trace => LogLevel::TRACE,
        }
    }
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::TRACE => Level::Trace,
            LogLevel::DEBUG => Level::Debug,
            LogLevel::INFO => Level::Info,
            LogLevel::WARN => Level::Warn,
            LogLevel::ERROR => Level::Error,
        }
    }
}
