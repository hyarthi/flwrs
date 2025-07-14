use crate::schema::schema::{
    field_type::Enum as FieldType, FieldDefinition as PbFieldDefinition,
    SchemaDefinition as PbSchemaDefinition,
};
use crate::schema::sink::Initialize;
use prost::alloc::string::String;

pub struct InitializeRequest {
    id: String,
    version: String,
    schema: SchemaDefinition,
}

impl InitializeRequest {
    pub fn new() -> Self {
        Self {
            id: String::new(),
            version: String::new(),
            schema: SchemaDefinition::new(),
        }
    }

    fn with_id(mut self, id: String) -> Self {
        self.id = id;
        self
    }

    fn with_version(mut self, version: String) -> Self {
        self.version = version;
        self
    }

    fn with_schema(mut self, schema: SchemaDefinition) -> Self {
        self.schema = schema;
        self
    }
}

impl Into<Initialize> for InitializeRequest {
    fn into(self) -> Initialize {
        Initialize {
            plugin_id: self.id,
            plugin_version: self.version,
            schema: Some(self.schema.into()),
        }
    }
}

impl Into<crate::schema::source::Initialize> for InitializeRequest {
    fn into(self) -> crate::schema::source::Initialize {
        crate::schema::source::Initialize {
            plugin_id: self.id,
            plugin_version: self.version,
            schema: Some(self.schema.into()),
        }
    }
}

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

impl Into<PbSchemaDefinition> for SchemaDefinition {
    fn into(self) -> PbSchemaDefinition {
        PbSchemaDefinition {
            fields: self.fields.into_iter().map(|field| field.into()).collect(),
        }
    }
}

pub struct FieldDefinition {
    key: String,
    description: Option<String>,
    type_: FieldType,
}

impl FieldDefinition {
    pub fn new() -> Self {
        Self {
            key: String::new(),
            description: None,
            type_: FieldType::String,
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
}

impl Into<PbFieldDefinition> for FieldDefinition {
    fn into(self) -> PbFieldDefinition {
        PbFieldDefinition {
            key: self.key,
            r#type: self.type_ as i32,
            description: match self.description {
                None => String::new(),
                Some(str) => String::from(str),
            },
        }
    }
}

pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
}
