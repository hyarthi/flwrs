use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::MSG_CLIENT;
use crate::schema::common::log_level::Enum as LogLevel;
use crate::schema::sink::sink_message::Payload;
use crate::schema::sink::{
    runtime_sink_message::Payload as RuntimeSinkMessagePayload, RuntimeSinkMessage, SinkMessage,
};
use crate::sink::plugin::Sink;
use prost::Message;
use std::fmt::format;

pub struct SinkRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
}

pub struct SinkRunner<T>
where
    T: Sink,
{
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
}

impl<T> SinkRunner<T>
where
    T: Sink,
{
    #[allow(dead_code)]
    pub async fn initialize(plugin: T, config: SinkRunnerConfig) -> Result<Self, Error> {
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
            plugin,
            plugin_id: id,
            log_level,
        }
    }

    #[allow(dead_code)]
    pub async fn run(&mut self) -> Result<(), Error> {
        // send hello to runtime
        let payload = match self
            .plugin
            .initialize(self.plugin_id.clone(), self.log_level)
        {
            Ok(result) => result,
            Err(err) => {
                log::error!("Error initializing plugin: {}", err);
                return Err(Error::InitError(err));
            }
        };
        let hello_msg = SinkMessage {
            payload: Some(Payload::Initialize(payload.into())),
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
            let msg = match RuntimeSinkMessage::decode(bytes) {
                Ok(message) => message,
                Err(err) => {
                    log::error!("Error parsing message: {}", err);
                    continue;
                }
            };
            let pyld = match msg.payload {
                Some(payload) => payload,
                None => {
                    log::error!("Message payload is missing");
                    continue;
                }
            };

            match pyld {
                RuntimeSinkMessagePayload::Initialize(_) => {
                    // noop for sink
                    continue;
                }
                RuntimeSinkMessagePayload::Event(payload) => {
                    log::debug!("Received event: {:?}", payload.plugin_id.clone());
                    let result = self.plugin.consume_event(payload);
                    if let Err(err) = result {
                        log::error!("Error processing event: {}", err);
                        continue;
                    }
                    continue;
                }
                RuntimeSinkMessagePayload::Shutdown(_) => {
                    log::debug!("Received shutdown message");
                    let result = self.plugin.shutdown();
                    if let Err(err) = result {
                        log::error!("Error shutting down: {}", err);
                        break Err(Error::ShutdownError(err));
                    }
                    log::info!("Plugin shutdown: {}", self.plugin_id);
                    break Ok(());
                }
            }
        }
    }
}
