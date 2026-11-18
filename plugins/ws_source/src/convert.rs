//! The node's out schema, from its AsyncAPI document: the common message
//! fields, and `channels["<address>"].{path_params, messages.<name>}` typed by
//! the messages' payload schemas (the kit's JSON Schema conversion: what
//! flwrs can't express is refused, naming where).

use crate::spec::Api;
use flwrs_plugin::schema::payload::field_type::Enum as FieldType;
use flwrs_plugin::schema::payload::{FieldDefinition, SchemaDefinition};
use flwrs_plugin_kit::schema::{convert, field};
use serde_json::Value;

fn described(mut f: FieldDefinition, description: &str) -> FieldDefinition {
    f.description = description.into();
    f
}

/// The fields every event has.
pub fn common_fields() -> Vec<FieldDefinition> {
    let strings = FieldDefinition {
        nested_type_definition: Some(Box::new(field("", FieldType::String))),
        ..field("", FieldType::Array)
    };
    let headers = FieldDefinition {
        nested_type_definition: Some(Box::new(strings)),
        ..field("headers", FieldType::Map)
    };
    vec![
        described(
            field("connection_id", FieldType::String),
            "The connection the message came on (a ULID)",
        ),
        described(
            field("path", FieldType::String),
            "The matched channel address (/devices/{id})",
        ),
        described(
            field("uri", FieldType::String),
            "The connection's path and query, as requested",
        ),
        described(field("message_type", FieldType::String), "text or binary"),
        described(
            field("message", FieldType::String),
            "The name of the message it matched in the document",
        ),
        described(field("data", FieldType::Bytes), "The message, as sent"),
        described(
            headers,
            "The upgrade request's headers (lowercase names; credentials left out)",
        ),
        described(
            field("remote_addr", FieldType::String),
            "The client's address",
        ),
        described(
            field("principal", FieldType::String),
            "Who connected: the token's label or the certificate's name",
        ),
        described(
            field("receive_time", FieldType::Datetime),
            "When the server received it",
        ),
    ]
}

/// The node's out schema.
pub fn out_schema(api: Option<&Api>) -> Result<SchemaDefinition, Vec<String>> {
    let mut fields = common_fields();
    let Some(api) = api else {
        return Ok(SchemaDefinition { fields });
    };
    let mut errors = vec![];
    let mut channels = vec![];
    for channel in &api.channels {
        let mut parts = vec![];
        if !channel.params.is_empty() {
            let params = channel
                .params
                .iter()
                .map(|p| field(p, FieldType::String))
                .collect();
            parts.push(FieldDefinition {
                object_fields: params,
                ..field("path_params", FieldType::Object)
            });
        }
        let mut typed = vec![];
        for message in channel.messages.iter().filter(|m| m.json) {
            let Some(ptr) = &message.payload else {
                continue;
            };
            match convert(&api.doc, ptr, 0, &mut vec![]) {
                Ok(mut f) => {
                    f.key = message.name.clone();
                    typed.push(f);
                }
                Err(e) => errors.extend(
                    e.into_iter()
                        .map(|e| format!("{}: message {:?}: {e}", channel.address, message.name)),
                ),
            }
        }
        if !typed.is_empty() {
            parts.push(FieldDefinition {
                object_fields: typed,
                ..field("messages", FieldType::Object)
            });
        }
        channels.push(FieldDefinition {
            object_fields: parts,
            ..field(&channel.address, FieldType::Object)
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    fields.push(described(
        FieldDefinition { object_fields: channels, ..field("channels", FieldType::Object) },
        "The message, typed by the document: channels[\"<address>\"].{path_params, messages.<name>}",
    ));
    Ok(SchemaDefinition { fields })
}

/// The out schema as JSON (what the configurator returns for `out_schema`).
pub fn to_json(schema: &SchemaDefinition) -> Value {
    serde_json::to_value(schema).expect("a schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{load, parse, tests::READINGS};

    #[test]
    fn channels_and_messages_are_typed() {
        let api = load(parse(&Value::String(READINGS.into())).unwrap()).unwrap();
        let json = to_json(&out_schema(Some(&api)).unwrap());
        let channels = json["fields"].as_array().unwrap().last().unwrap();
        assert_eq!(channels["key"], "channels");
        let readings = &channels["objectFields"][0];
        assert_eq!(readings["key"], "/devices/{deviceId}");
        assert_eq!(
            readings["objectFields"][0]["objectFields"][0]["key"],
            "deviceId"
        );
        let messages = &readings["objectFields"][1]["objectFields"];
        assert_eq!(messages[0]["key"], "reading");
        assert_eq!(messages[0]["objectFields"][0]["type"], "F64");
        assert_eq!(messages[1]["objectFields"][1]["type"], "BOOL");
        assert_eq!(
            out_schema(None).unwrap().fields.len(),
            common_fields().len()
        );
    }
}
