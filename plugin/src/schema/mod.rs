pub mod rules;

// Generated code (prost + pbjson): clippy findings there aren't ours to fix.
#[allow(clippy::all, clippy::module_inception)]
pub mod schema {
    include!(concat!(env!("OUT_DIR"), "/schema.schema.rs"));
    include!(concat!(env!("OUT_DIR"), "/schema.schema.serde.rs"));
}

#[allow(clippy::all)]
pub mod common {
    include!(concat!(env!("OUT_DIR"), "/schema.common.rs"));
    include!(concat!(env!("OUT_DIR"), "/schema.common.serde.rs"));
}

#[allow(clippy::all)]
pub mod sink {
    include!(concat!(env!("OUT_DIR"), "/schema.sink.rs"));
    include!(concat!(env!("OUT_DIR"), "/schema.sink.serde.rs"));
}

#[allow(clippy::all)]
pub mod source {
    include!(concat!(env!("OUT_DIR"), "/schema.source.rs"));
    include!(concat!(env!("OUT_DIR"), "/schema.source.serde.rs"));
}

#[allow(clippy::all)]
pub mod transform {
    include!(concat!(env!("OUT_DIR"), "/schema.transform.rs"));
    include!(concat!(env!("OUT_DIR"), "/schema.transform.serde.rs"));
}

#[cfg(test)]
mod tests {
    use super::common::{exit_code, log_level, msg_code, plugin_type};
    use super::schema::{Field, FieldValue, PluginPayload, field_type, field_value::Value};
    use prost::Message;

    /// Protocol convention: every enum's zero value is UNDEFINED, so a value
    /// that was never set can't be mistaken for a real one.
    #[test]
    fn enum_zero_values_are_undefined() {
        assert_eq!(field_type::Enum::default(), field_type::Enum::Undefined);
        assert_eq!(plugin_type::Enum::default(), plugin_type::Enum::Undefined);
        assert_eq!(log_level::Enum::default(), log_level::Enum::Undefined);
        assert_eq!(exit_code::Enum::default(), exit_code::Enum::Undefined);
        assert_eq!(msg_code::Enum::default(), msg_code::Enum::Undefined);
        for name in [
            field_type::Enum::Undefined.as_str_name(),
            plugin_type::Enum::Undefined.as_str_name(),
            log_level::Enum::Undefined.as_str_name(),
            exit_code::Enum::Undefined.as_str_name(),
            msg_code::Enum::Undefined.as_str_name(),
        ] {
            assert_eq!(name, "UNDEFINED");
        }
    }

    #[test]
    fn unset_value_is_null_and_64_bit_values_round_trip() {
        let payload = PluginPayload {
            fields: vec![
                Field {
                    key: "null".into(),
                    value: Some(FieldValue { value: None }),
                },
                Field {
                    key: "i64".into(),
                    value: Some(FieldValue {
                        value: Some(Value::I64(i64::MIN)),
                    }),
                },
                Field {
                    key: "u64".into(),
                    value: Some(FieldValue {
                        value: Some(Value::U64(u64::MAX)),
                    }),
                },
                Field {
                    key: "f64".into(),
                    value: Some(FieldValue {
                        value: Some(Value::F64(1e300)),
                    }),
                },
                Field {
                    key: "str".into(),
                    value: Some(FieldValue {
                        value: Some(Value::String(String::new())),
                    }),
                },
            ],
        };
        let decoded = PluginPayload::decode(payload.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, payload);
        // An explicitly set empty string is distinct from null.
        assert!(decoded.fields[4].value.as_ref().unwrap().value.is_some());
        assert!(decoded.fields[0].value.as_ref().unwrap().value.is_none());
    }
}
