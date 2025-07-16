use crate::schema::schema::{
    field_type::Enum as FieldType, FieldDefinition as PbFieldDefinition,
    SchemaDefinition as PbSchemaDefinition,
};
use crate::schema::sink::Initialize;
use prost::alloc::boxed::Box as PbBox;
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

    pub fn with_id(mut self, id: String) -> Self {
        self.id = id;
        self
    }

    pub fn with_version(mut self, version: String) -> Self {
        self.version = version;
        self
    }

    pub fn with_schema(mut self, schema: SchemaDefinition) -> Self {
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
            type_: FieldType::String,
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

impl Into<PbFieldDefinition> for FieldDefinition {
    fn into(self) -> PbFieldDefinition {
        PbFieldDefinition {
            key: self.key,
            r#type: self.type_ as i32,
            description: match self.description {
                None => String::new(),
                Some(str) => String::from(str),
            },
            nested_type_definition: match self.nested_type_definition {
                None => None,
                Some(t) => Some(PbBox::new(t.into())),
            },
            object_fields: match self.object_fields {
                None => Vec::new(),
                Some(fields) => fields.into_iter().map(|field| field.into()).collect(),
            },
        }
    }
}

impl Into<PbFieldDefinition> for Box<FieldDefinition> {
    fn into(self) -> PbFieldDefinition {
        PbFieldDefinition {
            key: self.key,
            r#type: self.type_ as i32,
            description: match self.description {
                None => String::new(),
                Some(str) => String::from(str),
            },
            nested_type_definition: match self.nested_type_definition {
                None => None,
                Some(t) => Some(PbBox::new(t.into())),
            },
            object_fields: match self.object_fields {
                None => Vec::new(),
                Some(fields) => fields.into_iter().map(|field| field.into()).collect(),
            },
        }
    }
}

pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
}
