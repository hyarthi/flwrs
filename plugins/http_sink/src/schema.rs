use flwrs_plugin::plugin::core::{FieldDefinition, SchemaDefinition};
use flwrs_plugin::schema::schema::field_type::Enum as FieldType;

pub(crate) const URL_KEY: &str = "url";
pub(crate) const METHOD_KEY: &str = "method";
pub(crate) const HEADERS_KEY: &str = "headers";
pub(crate) const BODY_KEY: &str = "body";

pub(crate) fn build_schema() -> SchemaDefinition {
    SchemaDefinition::new().with_fields(vec![
        FieldDefinition::new()
            .with_key(URL_KEY.into())
            .with_description("HTTP URL".into())
            .with_type(FieldType::String),
        FieldDefinition::new()
            .with_key(METHOD_KEY.into())
            .with_description("HTTP method".into())
            .with_type(FieldType::String),
        FieldDefinition::new()
            .with_key(HEADERS_KEY.into())
            .with_description("HTTP headers".into())
            .with_type(FieldType::Map)
            .with_nested_type_definition(
                FieldDefinition::new()
                    .with_type(FieldType::Array)
                    .with_nested_type_definition(
                        FieldDefinition::new().with_type(FieldType::String),
                    ),
            ),
        FieldDefinition::new()
            .with_key(BODY_KEY.into())
            .with_description("HTTP body".into())
            .with_type(FieldType::Bytes),
    ])
}
