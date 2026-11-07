//! The protocol messages as the hub uses them (protocol v2, `docs/protocol.md`).
//!
//! Data events stay encoded: the hub routes, queues and forwards the `Event`
//! body as bytes, with the routing metadata beside it (it travels in the
//! frame header). The body is decoded only where an edge maps the payload.

use crate::modules::plugin::manifest::PluginType;
use bytes::Bytes;
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::protocol::{
    Event, EventMetadata, Initialize, capability::Enum as Capability, msg_code::Enum as MsgCode,
    plugin_type::Enum as PbPluginType,
};
use prost::Message;

/// A plugin's `Initialize`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InitInfo {
    /// Instance id: the node id the plugin was started with, or a remote_id.
    pub instance_id: String,
    pub plugin_version: String,
    /// `None` for `UNDEFINED` or an unknown value.
    pub plugin_type: Option<PluginType>,
    pub token: String,
    /// Payloads the plugin accepts (sinks, transforms).
    pub in_schema: Option<SchemaDefinition>,
    /// Payloads the plugin emits (sources, transforms).
    pub out_schema: Option<SchemaDefinition>,
    pub min_protocol_version: u32,
    pub max_protocol_version: u32,
    pub capabilities: Vec<Capability>,
    pub max_unacked: u32,
}

impl From<Initialize> for InitInfo {
    fn from(i: Initialize) -> Self {
        let plugin_type = match i.plugin_type() {
            PbPluginType::Source => Some(PluginType::Source),
            PbPluginType::Sink => Some(PluginType::Sink),
            PbPluginType::Transform => Some(PluginType::Transform),
            PbPluginType::Undefined => None,
        };
        Self {
            instance_id: i.instance_id,
            plugin_version: i.plugin_version,
            plugin_type,
            token: i.token,
            in_schema: i.in_schema,
            out_schema: i.out_schema,
            min_protocol_version: i.min_protocol_version,
            max_protocol_version: i.max_protocol_version,
            // Unknown values (from a newer plugin) are simply not enabled.
            capabilities: i
                .capabilities
                .iter()
                .filter_map(|c| Capability::try_from(*c).ok())
                .collect(),
            max_unacked: i.max_unacked,
        }
    }
}

/// A data event: the encoded `Event` body, and the metadata that goes with it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct DataEvent {
    /// The protobuf `Event` (its payload), as received or as it will be sent.
    pub body: Bytes,
    /// From the hub to a plugin: the event's routing metadata. From a
    /// transform: the metadata of the input this output belongs to (`None`
    /// for a new trace). From a source: `None`.
    pub metadata: Option<EventMetadata>,
    /// Delivery attempt (1 for the first), for events the hub sends.
    pub attempt: u32,
}

impl DataEvent {
    pub(crate) fn from_payload(payload: PluginPayload) -> Self {
        Self {
            body: Event {
                payload: Some(payload),
            }
            .encode_to_vec()
            .into(),
            metadata: None,
            attempt: 0,
        }
    }

    /// Decodes the payload (for a mapping).
    pub(crate) fn payload(&self) -> Result<PluginPayload, prost::DecodeError> {
        Ok(Event::decode(self.body.clone())?.payload.unwrap_or_default())
    }

    /// The body size, which queue limits count.
    pub(crate) fn size(&self) -> usize {
        self.body.len()
    }
}

#[cfg(test)]
impl DataEvent {
    /// Tests label events by their body.
    pub(crate) fn label(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Capabilities in use on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Negotiated {
    /// ACK / NACK and redelivery.
    pub acks: bool,
    /// Credit-based flow control for what the plugin produces.
    pub credits: bool,
    /// With `acks`, for consumers: the most unacked events in flight.
    pub max_unacked: u32,
}

impl Negotiated {
    pub(crate) fn capabilities(&self) -> Vec<i32> {
        let mut caps = vec![];
        if self.acks {
            caps.push(Capability::Acks as i32);
        }
        if self.credits {
            caps.push(Capability::Credits as i32);
        }
        caps
    }
}

/// Whether a plugin of `plugin_type` may send frames with `code` once it's
/// accepted (`docs/protocol.md`, section 4.2).
pub(crate) fn plugin_may_send(plugin_type: PluginType, code: MsgCode) -> bool {
    match code {
        MsgCode::Log | MsgCode::Error | MsgCode::Exit | MsgCode::Ping | MsgCode::Pong => true,
        MsgCode::Event => plugin_type != PluginType::Sink,
        MsgCode::Ack | MsgCode::Nack => plugin_type != PluginType::Source,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_by_plugin_type() {
        use PluginType::*;
        assert!(plugin_may_send(Source, MsgCode::Event));
        assert!(!plugin_may_send(Source, MsgCode::Ack));
        assert!(!plugin_may_send(Sink, MsgCode::Event));
        assert!(plugin_may_send(Sink, MsgCode::Nack));
        assert!(plugin_may_send(Transform, MsgCode::Event));
        assert!(plugin_may_send(Transform, MsgCode::Ack));
        for t in [Source, Sink, Transform] {
            for code in [
                MsgCode::Initialize,
                MsgCode::InitializeResponse,
                MsgCode::Shutdown,
                MsgCode::Credit,
            ] {
                assert!(!plugin_may_send(t, code), "{t:?} {code:?}");
            }
        }
    }

    #[test]
    fn payload_round_trip() {
        use flwrs_plugin::schema::payload::{Field, FieldValue, field_value::Value};
        let payload = PluginPayload {
            fields: vec![Field {
                key: "k".into(),
                value: Some(FieldValue {
                    value: Some(Value::U64(7)),
                }),
            }],
        };
        let event = DataEvent::from_payload(payload.clone());
        assert_eq!(event.payload().unwrap(), payload);
        assert_eq!(event.size(), event.body.len());
        assert!(DataEvent::default().payload().unwrap().fields.is_empty());
    }
}
