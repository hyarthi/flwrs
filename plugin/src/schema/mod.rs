//! The protocol's protobuf messages (generated from `plugin/schema/*.proto`)
//! and the payload schema rules.

pub mod rules;

// Generated code (prost + pbjson): clippy findings there aren't ours to fix.

/// `flwrs.payload`: the typed payload data model.
#[allow(clippy::all)]
pub mod payload {
    include!(concat!(env!("OUT_DIR"), "/flwrs.payload.rs"));
    include!(concat!(env!("OUT_DIR"), "/flwrs.payload.serde.rs"));
}

/// `flwrs.protocol`: the frame header and every protocol message.
#[allow(clippy::all)]
pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/flwrs.protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/flwrs.protocol.serde.rs"));
}

#[cfg(test)]
mod tests {
    use super::payload::{Field, FieldValue, PackedF32, PluginPayload, field_type, field_value::Value};
    use super::protocol::{capability, exit_code, log_level, msg_code, plugin_type};
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
        assert_eq!(capability::Enum::default(), capability::Enum::Undefined);
        for name in [
            field_type::Enum::Undefined.as_str_name(),
            plugin_type::Enum::Undefined.as_str_name(),
            log_level::Enum::Undefined.as_str_name(),
            exit_code::Enum::Undefined.as_str_name(),
            msg_code::Enum::Undefined.as_str_name(),
            capability::Enum::Undefined.as_str_name(),
        ] {
            assert_eq!(name, "UNDEFINED");
        }
    }

    fn field(key: &str, value: Option<Value>) -> Field {
        Field {
            key: key.into(),
            value: Some(FieldValue { value }),
        }
    }

    #[test]
    fn unset_value_is_null_and_64_bit_and_packed_values_round_trip() {
        let payload = PluginPayload {
            fields: vec![
                field("null", None),
                field("i64", Some(Value::I64(i64::MIN))),
                field("u64", Some(Value::U64(u64::MAX))),
                field("f64", Some(Value::F64(1e300))),
                field("str", Some(Value::String(String::new()))),
                field(
                    "embedding",
                    Some(Value::PackedF32(PackedF32 {
                        values: vec![0.5, -1.25, 3.0],
                    })),
                ),
            ],
        };
        let decoded = PluginPayload::decode(payload.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, payload);
        // An explicitly set empty string is distinct from null.
        assert!(decoded.fields[4].value.as_ref().unwrap().value.is_some());
        assert!(decoded.fields[0].value.as_ref().unwrap().value.is_none());
    }

    #[test]
    fn packed_arrays_are_packed_on_the_wire() {
        // 1000 f32s: 4 bytes each plus a few bytes of framing, versus at
        // least 7 per element as an ARRAY of FieldValues.
        let packed = FieldValue {
            value: Some(Value::PackedF32(PackedF32 {
                values: vec![1.0; 1000],
            })),
        };
        assert!(packed.encoded_len() < 4010, "{}", packed.encoded_len());
    }
}
