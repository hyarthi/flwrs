use crate::plugin::msg_client::MSG_CLIENT;
use crate::schema::common::log_level::Enum;
use crate::schema::common::{
    log_level::Enum as LogLevel, plugin_type::Enum as PluginType, LogEvent,
};
use lazy_static::lazy_static;
use log::{Level, Metadata, Record, SetLoggerError};
use prost::Message;
use std::sync::{Arc, RwLock};

lazy_static! {
    pub(crate) static ref LOGGER: Arc<RwLock<PluginLogger>> =
        Arc::new(RwLock::new(PluginLogger::default()));
    pub(crate) static ref LOG_WRAPPER: Arc<LogWrapper> = Arc::new(LogWrapper::default());
}

pub(crate) struct PluginLogger {
    plugin_id: String,
    plugin_type: PluginType,
    level: Level,
}

impl PluginLogger {
    pub(crate) fn set_plugin_id(&mut self, plugin_id: String) {
        self.plugin_id = plugin_id;
    }

    pub(crate) fn set_plugin_type(&mut self, plugin_type: PluginType) {
        self.plugin_type = plugin_type;
    }

    pub(crate) fn set_level(&mut self, level: Level) {
        self.level = level;
    }

    pub(crate) fn initialize(id: &str, log_level: Level) -> Result<(), SetLoggerError> {
        let mut logger = LOGGER.write().expect("Was expecting to lock");
        logger.set_plugin_id(id.into());
        logger.set_plugin_type(PluginType::Sink);
        logger.set_level(log_level.into());
        log::set_logger(LOG_WRAPPER.as_ref())?;
        Ok(())
    }
}

impl log::Log for PluginLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        let log_level: LogLevel = self.level.into();
        let msg = LogEvent {
            plugin_id: self.plugin_id.to_string(),
            plugin_type: self.plugin_type as i32,
            log_level: log_level as i32,
            message: record.args().as_str().unwrap_or("<no message>").to_string(),
            details: vec![],
        };

        tokio::task::spawn(async move {
            match MSG_CLIENT
                .read()
                .await
                .send(msg.encode_to_vec().as_slice())
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    println!("Failed to send log message: {}", e); // TODO handle this better
                    println!("Message: {}", msg.message);
                    println!("Details: {:?}", msg.details);
                    println!("Log level: {:?}", log_level);
                    println!("Plugin type: {:?}", msg.plugin_type);
                    println!("Plugin ID: {:?}", msg.plugin_id);
                }
            };
        });
    }

    fn flush(&self) {
        // noop, we flush immediately
    }
}

impl Default for PluginLogger {
    fn default() -> Self {
        Self {
            plugin_id: "<no-ID>".to_string(),
            level: Level::Warn,
            plugin_type: PluginType::Undefined,
        }
    }
}

impl Into<LogLevel> for Level {
    fn into(self) -> LogLevel {
        match self {
            Level::Error => LogLevel::Error,
            Level::Warn => LogLevel::Warn,
            Level::Info => LogLevel::Info,
            Level::Debug => LogLevel::Debug,
            Level::Trace => LogLevel::Trace,
        }
    }
}

impl From<LogLevel> for Level {
    fn from(value: LogLevel) -> Self {
        match value {
            Enum::Undefined => Self::Warn,
            Enum::Trace => Self::Trace,
            Enum::Debug => Self::Debug,
            Enum::Info => Self::Info,
            Enum::Warn => Self::Warn,
            Enum::Error => Self::Error,
        }
    }
}

#[derive(Default)]
pub(crate) struct LogWrapper;

impl log::Log for LogWrapper {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let logger = LOGGER.read();
        match logger {
            Ok(logger) => logger.enabled(metadata),
            Err(err) => {
                println!("Failed to read logger: {}", err);
                false
            }
        }
    }

    fn log(&self, record: &Record) {
        let logger = LOGGER.read();
        match logger {
            Ok(logger) => logger.log(record),
            Err(err) => {
                println!("Failed to read logger: {}", err);
            }
        };
    }

    fn flush(&self) {
        let logger = LOGGER.read();
        match logger {
            Ok(logger) => logger.flush(),
            Err(err) => {
                println!("Failed to read logger: {}", err);
            }
        };
    }
}
