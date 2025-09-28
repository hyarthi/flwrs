use crate::plugin::core::ConnectionConfig;
use crate::plugin::error::Error;
use crate::plugin::logger::PluginLogger;
use crate::plugin::msg_client::MSG_CLIENT;
use crate::schema::common::log_level::Enum as LogLevel;
use crate::schema::transform::transform_message::Payload;
use crate::schema::transform::{
    runtime_transform_message::Payload as RuntimeTransformMessagePayload, RuntimeTransformMessage, TransformMessage,
};
use crate::transform::plugin::Transform;
use prost::Message;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct TransformRunnerConfig {
    pub plugin_id: String,
    pub log_level: LogLevel,
    pub hub_connection: ConnectionConfig,
}

pub struct TransformRunner<T>
where
    T: for<'a> Transform<'a>,
{
    plugin: T,
    plugin_id: String,
    log_level: LogLevel,
    local_sink: Arc<Mutex<crate::transform::local_sink::LocalSink>>,
}

impl<T> TransformRunner<T>
where
    T: for<'a> Transform<'a>,
{
    #[allow(dead_code)]
    pub async fn initialize(plugin: T, config: TransformRunnerConfig) -> Result<Self, Error> {
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
            plugin_id: id.clone(),
            log_level,
            local_sink: Arc::new(Mutex::new(crate::transform::local_sink::LocalSink::new(id))),
        }
    }

    #[allow(dead_code)]
    pub async fn run(&mut self) -> Result<(), Error> {
        // send hello to runtime
        let payload = match self.plugin.initialize(
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
        let hello_msg = TransformMessage {
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
            let msg = match RuntimeTransformMessage::decode(bytes) {
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
                RuntimeTransformMessagePayload::Initialize(_) => {
                    // noop for sink
                    continue;
                }
                RuntimeTransformMessagePayload::Event(payload) => {
                    log::debug!("Received event: {:?}", payload.plugin_id.clone());
                    let result = self.plugin.process_event(payload);
                    if let Err(err) = result {
                        log::error!("Error processing event: {}", err);
                        continue;
                    }
                    continue;
                }
                RuntimeTransformMessagePayload::Shutdown(_) => {
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
