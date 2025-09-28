use super::{
    ordered_log_levels, FormatType, LogError, LogLevel, LoggerConfig, SinkConfig, SinkType,
};
use flexi_logger::{Age, Cleanup, Criterion, FileSpec, LoggerHandle, Naming};
use log::{Level, Metadata, Record};
use std::marker::Send;
use syslog::Facility;

pub(crate) struct LogLogger {
    config: LoggerConfig,
    log_level_idx: usize,
    sinks: Vec<Sink>,
    flexi_handles: Vec<LoggerHandle>,
}

pub(crate) struct Sink {
    delegate: Box<dyn log::Log>,
}

unsafe impl Send for Sink {}

impl log::Log for Sink {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.delegate.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.delegate.log(record)
    }

    fn flush(&self) {
        self.delegate.flush()
    }
}

impl log::Log for LogLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let midx = ordered_log_levels.iter()
            .position(|l| l.clone() == LogLevel::from(metadata.level()))
            .unwrap_or(ordered_log_levels.len()
            );
        midx <= self.log_level_idx
    }
    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.sinks.iter().for_each(|sink| {
            sink.log(record);
        });
    }
    fn flush(&self) {
        self.sinks.iter().for_each(|sink| {
            sink.flush();
        });
    }
}

impl Default for LogLogger {
    fn default() -> Self {
        Self::new(LoggerConfig {
            id: "actor".to_string(),
            format: FormatType::PLAIN,
            level: LogLevel::INFO,
            sinks: vec![SinkConfig {
                sink_type: SinkType::CONSOLE,
                file_directory: None,
                file_max_size_bytes: None,
                file_max_log_history: None,
            }],
        })
            .unwrap_or_else(|_e| {
                println!("failed to create default logger!");
                LogLogger {
                    config: LoggerConfig {
                        id: "actor".to_string(),
                        format: FormatType::PLAIN,
                        level: LogLevel::INFO,
                        sinks: vec![],
                    },
                    log_level_idx: ordered_log_levels.clone().into_iter()
                        .position(|l| l.clone() == LogLevel::INFO)
                        .unwrap_or(2),
                    sinks: vec![],
                    flexi_handles: vec![],
                }
            })
    }
}

impl LogLogger {
    pub fn new(config: LoggerConfig) -> Result<Self, LogError> {
        let mut sinks = vec![];
        let mut handles = vec![];
        for sink_cfg in &config.sinks {
            match sink_cfg.sink_type {
                SinkType::CONSOLE => {
                    let (sink, handle) = flexi_logger::Logger::try_with_str(config.level.stringify().as_str())?
                        .log_to_stdout()
                        .format(flexi_logger::colored_with_thread)
                        .build()?;
                    sinks.push(Sink { delegate: sink });
                    handles.push(handle);
                }
                SinkType::FILE => {
                    let (sink, handle) = flexi_logger::Logger::try_with_str(config.level.stringify().as_str())?
                        .log_to_file(
                            FileSpec::default()
                                .basename("actor_log")
                                .directory(sink_cfg.file_directory.clone().unwrap_or("logs/".to_string())),
                        )
                        .format(flexi_logger::colored_with_thread)
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
                    sinks.push(Sink { delegate: sink });
                    handles.push(handle);
                }
                SinkType::SYSLOG => {
                    let sink = match syslog::unix(syslog::Formatter3164 {
                        facility: Facility::LOG_USER,
                        hostname: None,
                        process: "actor".to_string(),
                        pid: 0,
                    }) {
                        Ok(l) => Box::new(syslog::BasicLogger::new(l)),
                        Err(e) => {
                            return Err(LogError::SyslogError(e))
                        }
                    };
                    sinks.push(Sink { delegate: sink });
                }
            };
        }
        let idx = ordered_log_levels.iter()
            .position(|&l| l == config.level)
            .ok_or_else(|| { return LogError::LogLevelNotFoundError(config.level); })?;

        Ok(LogLogger {
            config,
            log_level_idx: idx,
            sinks,
            flexi_handles: handles,
        })
    }
}

impl From<Level> for LogLevel {
    fn from(level: log::Level) -> Self {
        match level {
            Level::Error => LogLevel::ERROR,
            Level::Warn => LogLevel::WARN,
            Level::Info => LogLevel::INFO,
            Level::Debug => LogLevel::DEBUG,
            Level::Trace => LogLevel::TRACE,
        }
    }
}

impl Into<Level> for LogLevel {
    fn into(self) -> Level {
        match self {
            LogLevel::TRACE => log::Level::Trace,
            LogLevel::DEBUG => log::Level::Debug,
            LogLevel::INFO => log::Level::Info,
            LogLevel::WARN => log::Level::Warn,
            LogLevel::ERROR => log::Level::Error,
        }
    }
}
