use crate::schema::schema::{
    FieldDefinition as PbFieldDefinition, SchemaDefinition as PbSchemaDefinition,
    field_type::Enum as FieldType,
};
use crate::schema::sink::Initialize as SinkInitialize;
use crate::schema::source::Initialize as SourceInitialize;
use crate::schema::transform::Initialize as TransformInitialize;
use prost::alloc::boxed::Box as PbBox;
use std::time::Duration;

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

impl From<InitializeRequest> for SinkInitialize {
    fn from(req: InitializeRequest) -> Self {
        SinkInitialize {
            plugin_id: req.id,
            plugin_version: req.version,
            schema: Some(req.in_schema.into()),
            token: String::new(), // set by the runner
        }
    }
}

impl From<InitializeRequest> for SourceInitialize {
    fn from(req: InitializeRequest) -> Self {
        SourceInitialize {
            plugin_id: req.id,
            plugin_version: req.version,
            schema: Some(req.out_schema.into()),
            token: String::new(), // set by the runner
        }
    }
}

impl From<InitializeRequest> for TransformInitialize {
    fn from(req: InitializeRequest) -> Self {
        TransformInitialize {
            plugin_id: req.id,
            plugin_version: req.version,
            in_schema: Some(req.in_schema.into()),
            out_schema: Some(req.out_schema.into()),
            token: String::new(), // set by the runner
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
}

impl FieldDefinition {
    pub fn new() -> Self {
        Self {
            key: String::new(),
            description: None,
            type_: FieldType::Undefined,
            nested_type_definition: None,
            object_fields: None,
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
}

impl ConnectionConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            token: None,
            reconnect: ReconnectPolicy::default(),
        }
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
