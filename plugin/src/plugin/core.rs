use crate::plugin::frame::PROTOCOL_VERSION;
use crate::plugin::msg_client::{Connector, HubClient};
use crate::schema::payload::{
    FieldDefinition as PbFieldDefinition, SchemaDefinition as PbSchemaDefinition,
    field_type::Enum as FieldType,
};
use crate::schema::payload::PluginPayload;
use crate::schema::protocol::{
    EventMetadata, Initialize, capability::Enum as Capability, plugin_type::Enum as PluginType,
};
use prost::alloc::boxed::Box as PbBox;
use std::time::Duration;

/// A data event received from the hub.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InboundEvent {
    /// The hub's sequence number for this event on this connection.
    pub seq: u64,
    /// Delivery attempt: 1 for the first; higher means it may be a duplicate.
    pub attempt: u32,
    /// The hub's routing metadata (event id, trace id, hops, …).
    pub metadata: Option<EventMetadata>,
    pub payload: PluginPayload,
    /// The connection it arrived on (seq numbers are per connection).
    pub(crate) conn_id: u64,
}

impl InboundEvent {
    /// A reference to this event, for a transform that emits an output for it
    /// later (an accumulator), through `LocalSink::emit_linked`.
    pub fn input_ref(&self) -> InputRef {
        InputRef {
            conn_id: self.conn_id,
            seq: self.seq,
        }
    }
}

/// Which input an output belongs to (see [`InboundEvent::input_ref`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRef {
    pub(crate) conn_id: u64,
    pub(crate) seq: u64,
}

pub struct InitializeRequest {
    id: String,
    version: String,
    in_schema: SchemaDefinition,
    out_schema: SchemaDefinition,
}

impl InitializeRequest {
    pub fn new() -> Self {
        Self {
            id: String::new(),
            version: String::new(),
            in_schema: SchemaDefinition::new(),
            out_schema: SchemaDefinition::new(),
        }
    }

    pub fn with_id(mut self, id: String) -> Self {
        self.id = id;
        self
    }

    pub fn with_version(mut self, version: String) -> Self {
        self.version = version;
        self
    }

    /// Sets both the input and output schema. Use this for sinks (input) and
    /// sources (output); transforms should set each side explicitly.
    pub fn with_schema(mut self, schema: SchemaDefinition) -> Self {
        self.in_schema = schema.clone();
        self.out_schema = schema;
        self
    }

    /// Schema of payloads the plugin accepts (sinks, transforms).
    pub fn with_in_schema(mut self, schema: SchemaDefinition) -> Self {
        self.in_schema = schema;
        self
    }

    /// Schema of payloads the plugin produces (sources, transforms).
    pub fn with_out_schema(mut self, schema: SchemaDefinition) -> Self {
        self.out_schema = schema;
        self
    }
}

impl Default for InitializeRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl InitializeRequest {
    /// The protocol message for a plugin of `plugin_type`. The runner fills in
    /// the token and the consumer window. Only the schemas the type uses are
    /// sent: `in` for sinks and transforms, `out` for sources and transforms.
    pub(crate) fn into_initialize(self, plugin_type: PluginType) -> Initialize {
        let (in_schema, out_schema) = match plugin_type {
            PluginType::Sink => (Some(self.in_schema.into()), None),
            PluginType::Source => (None, Some(self.out_schema.into())),
            _ => (Some(self.in_schema.into()), Some(self.out_schema.into())),
        };
        Initialize {
            instance_id: self.id,
            plugin_version: self.version,
            plugin_type: plugin_type as i32,
            token: String::new(),
            in_schema,
            out_schema,
            min_protocol_version: PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            capabilities: vec![Capability::Acks as i32, Capability::Credits as i32],
            max_unacked: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct SchemaDefinition {
    fields: Vec<FieldDefinition>,
}

impl SchemaDefinition {
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn add_field(mut self, field: FieldDefinition) -> Self {
        self.fields.push(field);
        self
    }

    pub fn with_fields(mut self, fields: Vec<FieldDefinition>) -> Self {
        self.fields = fields;
        self
    }

    pub fn remove_field(mut self, idx: usize) -> Self {
        self.fields.remove(idx);
        self
    }
}

impl From<SchemaDefinition> for PbSchemaDefinition {
    fn from(schema: SchemaDefinition) -> Self {
        PbSchemaDefinition {
            fields: schema
                .fields
                .into_iter()
                .map(|field| field.into())
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct FieldDefinition {
    key: String,
    description: Option<String>,
    type_: FieldType,
    nested_type_definition: Option<Box<FieldDefinition>>,
    object_fields: Option<Vec<FieldDefinition>>,
    content_type: Option<String>,
}

impl FieldDefinition {
    pub fn new() -> Self {
        Self {
            key: String::new(),
            description: None,
            type_: FieldType::Undefined,
            nested_type_definition: None,
            object_fields: None,
            content_type: None,
        }
    }

    pub fn with_key(mut self, key: String) -> Self {
        self.key = key;
        self
    }

    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    pub fn with_type(mut self, type_: FieldType) -> Self {
        self.type_ = type_;
        self
    }

    pub fn with_nested_type_definition(mut self, definition: FieldDefinition) -> Self {
        self.nested_type_definition = Some(Box::new(definition));
        self
    }

    pub fn with_object_fields(mut self, fields: Vec<FieldDefinition>) -> Self {
        self.object_fields = Some(fields);
        self
    }

    /// BYTES only: the media type of the bytes, e.g. `image/jpeg`.
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
}

impl From<FieldDefinition> for PbFieldDefinition {
    fn from(field: FieldDefinition) -> Self {
        PbFieldDefinition {
            key: field.key,
            r#type: field.type_ as i32,
            description: field.description.unwrap_or_default(),
            nested_type_definition: field
                .nested_type_definition
                .map(|t| PbBox::new((*t).into())),
            object_fields: field
                .object_fields
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            content_type: field.content_type.unwrap_or_default(),
        }
    }
}

impl Default for FieldDefinition {
    fn default() -> Self {
        Self::new()
    }
}

/// Environment variable a plugin reads its connection token from.
pub const TOKEN_ENV: &str = "FLWRS_TOKEN";

/// Longest wait between reconnect attempts that the protocol allows. The hub's
/// reconnect timeout is set above this, so a reconnecting plugin isn't killed.
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// How to reach the hub and authenticate to it.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    /// Connection token. If `None`, the runner reads [`TOKEN_ENV`]. Prefer the
    /// environment over a command-line argument: arguments show up in `ps`.
    pub token: Option<String>,
    pub reconnect: ReconnectPolicy,
    /// Connect to the hub's TLS listener. `None` = from the `FLWRS_TLS_*`
    /// environment variables, if `FLWRS_TLS_CA` is set; else plain TCP.
    #[cfg(feature = "tls")]
    pub tls: Option<crate::plugin::tls::TlsSettings>,
}

impl ConnectionConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            token: None,
            reconnect: ReconnectPolicy::default(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// Connect over TLS (see [`crate::plugin::tls::TlsSettings`]).
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, tls: crate::plugin::tls::TlsSettings) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    pub fn with_reconnect(mut self, reconnect: ReconnectPolicy) -> Self {
        self.reconnect = reconnect;
        self
    }

    pub(crate) fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The connection a runner for `plugin_type` uses.
    pub(crate) fn client(
        &self,
        plugin_type: PluginType,
    ) -> Result<std::sync::Arc<HubClient>, crate::plugin::error::Error> {
        #[cfg(feature = "tls")]
        let connector = match self
            .tls
            .clone()
            .or_else(crate::plugin::tls::TlsSettings::from_env)
        {
            Some(settings) => Connector::Tls(std::sync::Arc::new(
                crate::plugin::tls::ClientTls::new(&settings, &self.host)?,
            )),
            None => Connector::Plain,
        };
        #[cfg(not(feature = "tls"))]
        let connector = Connector::Plain;
        Ok(HubClient::new(self.addr(), plugin_type, connector))
    }

    /// The configured token, else [`TOKEN_ENV`], else empty (the hub will
    /// reject the connection).
    pub(crate) fn resolved_token(&self) -> String {
        self.token
            .clone()
            .or_else(|| std::env::var(TOKEN_ENV).ok())
            .unwrap_or_default()
    }
}

/// Backoff between attempts to (re)connect to the hub: starts at
/// `initial_delay`, doubles after each failed attempt up to `max_delay`
/// (capped at [`MAX_RECONNECT_DELAY`]), with ±10 % jitter. It resets once the
/// hub has accepted the plugin (`InitializeResponse`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: MAX_RECONNECT_DELAY,
        }
    }
}
