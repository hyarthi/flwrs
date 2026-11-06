//! Maps the per-type protocol messages (source / sink / transform) onto one
//! shape, so the rest of the hub handles all plugin types the same way.

use crate::modules::plugin::manifest::PluginType;
use flwrs_plugin::plugin::frame::{Frame, FrameError, encode_message};
use flwrs_plugin::schema::common::{
    ErrorEvent, EventMetadata, Exit, InitializeResponse, LogEvent, Shutdown,
    msg_code::Enum as MsgCode,
};
use flwrs_plugin::schema::schema::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::{sink, source, transform};

/// A plugin's `Initialize`, independent of its type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InitInfo {
    /// Instance id: the node id the plugin was started with.
    pub instance_id: String,
    pub plugin_version: String,
    pub token: String,
    /// Payloads the plugin accepts (sinks, transforms).
    pub in_schema: Option<SchemaDefinition>,
    /// Payloads the plugin emits (sources, transforms).
    pub out_schema: Option<SchemaDefinition>,
}

/// A data event, independent of its type (`SourceEvent`, `SinkEvent`,
/// `TransformEvent` have the same fields).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct DataEvent {
    pub instance_id: String,
    pub plugin_version: String,
    pub payload: Option<PluginPayload>,
    pub metadata: Option<EventMetadata>,
}

/// Plugin → runtime.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PluginMsg {
    Initialize(InitInfo),
    Event(DataEvent),
    Log(LogEvent),
    Error(ErrorEvent),
    Exit(Exit),
    /// No (or an unknown) `oneof` payload, e.g. from a newer plugin.
    Empty,
}

/// Runtime → plugin.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RuntimeMsg {
    /// `InitializeResponse`: the hub accepted the plugin.
    Accept,
    Shutdown,
    /// A data event for a sink or transform.
    Event(DataEvent),
}

/// The plugin type that sends frames with `code`, for plugin → runtime codes.
pub(crate) fn sender_type(code: MsgCode) -> Option<PluginType> {
    match code {
        MsgCode::SourceMessage => Some(PluginType::Source),
        MsgCode::SinkMessage => Some(PluginType::Sink),
        MsgCode::TransformMessage => Some(PluginType::Transform),
        _ => None,
    }
}

/// The code a plugin of `plugin_type` must send.
pub(crate) fn plugin_code(plugin_type: PluginType) -> MsgCode {
    match plugin_type {
        PluginType::Source => MsgCode::SourceMessage,
        PluginType::Sink => MsgCode::SinkMessage,
        PluginType::Transform => MsgCode::TransformMessage,
    }
}

/// Decodes a frame from a plugin of `plugin_type`. The frame's code must be
/// that type's plugin → runtime code.
pub(crate) fn decode(plugin_type: PluginType, frame: Frame) -> Result<PluginMsg, FrameError> {
    Ok(match plugin_type {
        PluginType::Source => {
            use source::source_message::Payload;
            match frame.decode::<source::SourceMessage>()?.payload {
                Some(Payload::Initialize(i)) => PluginMsg::Initialize(InitInfo {
                    instance_id: i.plugin_id,
                    plugin_version: i.plugin_version,
                    token: i.token,
                    in_schema: None,
                    out_schema: i.schema,
                }),
                Some(Payload::Event(e)) => PluginMsg::Event(DataEvent {
                    instance_id: e.plugin_id,
                    plugin_version: e.plugin_version,
                    payload: e.payload,
                    metadata: e.metadata,
                }),
                Some(Payload::Log(l)) => PluginMsg::Log(l),
                Some(Payload::Error(e)) => PluginMsg::Error(e),
                Some(Payload::Exit(e)) => PluginMsg::Exit(e),
                None => PluginMsg::Empty,
            }
        }
        PluginType::Sink => {
            use sink::sink_message::Payload;
            match frame.decode::<sink::SinkMessage>()?.payload {
                Some(Payload::Initialize(i)) => PluginMsg::Initialize(InitInfo {
                    instance_id: i.plugin_id,
                    plugin_version: i.plugin_version,
                    token: i.token,
                    in_schema: i.schema,
                    out_schema: None,
                }),
                Some(Payload::Log(l)) => PluginMsg::Log(l),
                Some(Payload::Error(e)) => PluginMsg::Error(e),
                Some(Payload::Exit(e)) => PluginMsg::Exit(e),
                None => PluginMsg::Empty,
            }
        }
        PluginType::Transform => {
            use transform::transform_message::Payload;
            match frame.decode::<transform::TransformMessage>()?.payload {
                Some(Payload::Initialize(i)) => PluginMsg::Initialize(InitInfo {
                    instance_id: i.plugin_id,
                    plugin_version: i.plugin_version,
                    token: i.token,
                    in_schema: i.in_schema,
                    out_schema: i.out_schema,
                }),
                Some(Payload::Event(e)) => PluginMsg::Event(DataEvent {
                    instance_id: e.plugin_id,
                    plugin_version: e.plugin_version,
                    payload: e.payload,
                    metadata: e.metadata,
                }),
                Some(Payload::Log(l)) => PluginMsg::Log(l),
                Some(Payload::Error(e)) => PluginMsg::Error(e),
                Some(Payload::Exit(e)) => PluginMsg::Exit(e),
                None => PluginMsg::Empty,
            }
        }
    })
}

/// Encodes a frame for a plugin of `plugin_type`. Fails for an event sent to
/// a source (sources receive no data).
pub(crate) fn encode(plugin_type: PluginType, msg: RuntimeMsg) -> Result<Vec<u8>, String> {
    Ok(match plugin_type {
        PluginType::Source => {
            use source::runtime_source_message::Payload;
            let payload = match msg {
                RuntimeMsg::Accept => Payload::Initialize(InitializeResponse {}),
                RuntimeMsg::Shutdown => Payload::Shutdown(Shutdown {}),
                RuntimeMsg::Event(_) => return Err("a source doesn't receive events".into()),
            };
            encode_message(&source::RuntimeSourceMessage {
                payload: Some(payload),
            })
        }
        PluginType::Sink => {
            use sink::runtime_sink_message::Payload;
            let payload = match msg {
                RuntimeMsg::Accept => Payload::Initialize(InitializeResponse {}),
                RuntimeMsg::Shutdown => Payload::Shutdown(Shutdown {}),
                RuntimeMsg::Event(e) => Payload::Event(sink::SinkEvent {
                    plugin_id: e.instance_id,
                    plugin_version: e.plugin_version,
                    payload: e.payload,
                    metadata: e.metadata,
                }),
            };
            encode_message(&sink::RuntimeSinkMessage {
                payload: Some(payload),
            })
        }
        PluginType::Transform => {
            use transform::runtime_transform_message::Payload;
            let payload = match msg {
                RuntimeMsg::Accept => Payload::Initialize(InitializeResponse {}),
                RuntimeMsg::Shutdown => Payload::Shutdown(Shutdown {}),
                RuntimeMsg::Event(e) => Payload::Event(transform::TransformEvent {
                    plugin_id: e.instance_id,
                    plugin_version: e.plugin_version,
                    payload: e.payload,
                    metadata: e.metadata,
                }),
            };
            encode_message(&transform::RuntimeTransformMessage {
                payload: Some(payload),
            })
        }
    })
}
