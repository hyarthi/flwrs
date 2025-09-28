use crate::plugin::error::TransformError;
use crate::plugin::msg_client::MSG_CLIENT;
use crate::schema::source::source_message::Payload;
use crate::schema::source::{SourceEvent, SourceMessage};
use prost::Message;

pub struct LocalSink {
    plugin_id: String,
}

impl LocalSink {
    pub fn new(plugin_id: String) -> Self {
        Self { plugin_id }
    }

    pub async fn event(&self, evt: SourceEvent) -> Result<(), TransformError> {
        let msg = SourceMessage {
            payload: Some(Payload::Event(evt)),
        };
        match MSG_CLIENT
            .read()
            .await
            .send(msg.encode_to_vec().as_slice())
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                log::error!("Error sending message: {}", err);
                Err(TransformError {
                    source: Box::new(err),
                })
            }
        }
    }
}
