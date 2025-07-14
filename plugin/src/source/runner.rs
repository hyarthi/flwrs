use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::{PluginLogger, LOGGER, LOG_WRAPPER};
use crate::plugin::msg_client::MSG_CLIENT;
use crate::schema::common::log_level::Enum as LogLevel;
use crate::schema::source::runtime_source_message::Payload;
use crate::schema::source::{RuntimeSourceMessage, SourceMessage};
use crate::source::local_sink::LocalSink;
use crate::source::plugin::Source;
use prost::Message;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct SourceRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
}

pub struct SourceRunner<T>
where
    T: for<'a> Source<'a>,
{
    plugin: RwLock<T>,
    plugin_id: String,
    log_level: LogLevel,
    local_sink: Arc<Mutex<LocalSink>>,
}

impl<T> SourceRunner<T>
where
    T: for<'a> Source<'a>,
{
    #[allow(dead_code)]
    pub async fn initialize(plugin: T, config: SourceRunnerConfig) -> Result<Self, Error> {
        MSG_CLIENT
            .write()
            .await
            .connect(
                format!(
                    "{}:{}",
                    config.hub_connection.host, config.hub_connection.port
                )
                .as_str(),
            )
            .await?;
        PluginLogger::initialize(config.plugin_id.as_str(), config.log_level.into())?;
        Ok(Self::new(config.plugin_id, plugin, config.log_level))
    }

    pub(crate) fn new(id: String, plugin: T, log_level: LogLevel) -> Self {
        Self {
            plugin: RwLock::new(plugin),
            log_level,
            plugin_id: id.clone(),
            local_sink: Arc::new(Mutex::new(LocalSink::new(id))),
        }
    }

    #[allow(dead_code)]
    pub async fn run(&self) -> Result<(), Error> {
        // send hello to runtime
        let payload = match self.plugin.write().await.initialize(
            self.plugin_id.clone(),
            self.log_level,
            self.local_sink.as_ref(),
        ) {
            Ok(result) => result,
            Err(err) => {
                log::error!("Error initializing plugin: {}", err);
                return Err(Error::InitError(err));
            }
        };
        let hello_msg = SourceMessage {
            payload: Some(crate::schema::source::source_message::Payload::Initialize(
                payload.into(),
            )),
        };

        match MSG_CLIENT
            .read()
            .await
            .send(hello_msg.encode_to_vec().as_slice())
            .await
        {
            Ok(_) => {}
            Err(err) => {
                log::error!("Error sending hello message: {}", err);
                return Err(Error::IOError(err));
            }
        };

        loop {
            let join_result = tokio::try_join!(self.consume_loop(), self.run_plugin());
            match join_result {
                Ok(_) => {
                    // both exited cleanly
                    break Ok(());
                }
                Err(err) => {
                    // if plugin crashed - try again
                    if let Error::SourceError(_) = err {
                        log::error!("Plugin crashed: {}", err.to_string());
                        continue;
                    }
                    // consume loop crashed - exit
                    let result = self.plugin.write().await.shutdown();
                    if let Err(err) = result {
                        log::error!("Error shutting down: {}", err);
                        return Err(Error::ShutdownError(err));
                    }
                    break Err(err);
                }
            }
        }
    }

    async fn consume_loop(&self) -> Result<(), Error> {
        loop {
            let bytes = match MSG_CLIENT.read().await.receive().await {
                Ok(bytes) => match bytes {
                    None => {
                        continue;
                    }
                    Some(bytes) => bytes,
                },
                Err(err) => {
                    log::error!("Error receiving message: {}", err);
                    continue;
                }
            };
            let msg_result = RuntimeSourceMessage::decode(bytes);
            if let Err(err) = msg_result {
                log::error!("Error parsing message: {}", err);
                continue;
            }
            let msg = msg_result.expect("Message should be valid");

            if msg.payload.is_none() {
                log::error!("Message payload is missing");
                continue;
            }

            match msg.payload.unwrap() {
                Payload::Initialize(_) => {}
                Payload::Shutdown(_) => {
                    log::debug!("Received shutdown message");
                    let result = self.plugin.write().await.shutdown();
                    if let Err(err) = result {
                        log::error!("Error shutting down: {}", err);
                        return Err(Error::ShutdownError(err));
                    }
                    log::info!("Plugin shutdown: {}", self.plugin_id);
                    return Ok(());
                }
            }
        }
    }

    async fn run_plugin(&self) -> Result<(), Error> {
        if let Err(err) = self.plugin.read().await.run() {
            return Err(Error::SourceError(err));
        }
        Ok(())
    }
}
