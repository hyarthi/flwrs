//! The out schema and the tick payload.

use crate::ticker::Tick;
use flwrs_plugin::plugin::core::{FieldDefinition, SchemaDefinition};
use flwrs_plugin::schema::payload::field_type::Enum as FieldType;
use flwrs_plugin::schema::payload::field_value::Value;
use flwrs_plugin::schema::payload::{Field, FieldValue, PluginPayload};
use jiff::Timestamp;

pub(crate) const TICK_TIME_KEY: &str = "tick_time";
pub(crate) const EMIT_TIME_KEY: &str = "emit_time";
pub(crate) const SEQ_KEY: &str = "seq";
pub(crate) const MISSED_KEY: &str = "missed";
pub(crate) const LABEL_KEY: &str = "label";

pub(crate) fn build_schema() -> SchemaDefinition {
    let field = |key: &str, description: &str, t: FieldType| {
        FieldDefinition::new()
            .with_key(key.into())
            .with_description(description.into())
            .with_type(t)
    };
    SchemaDefinition::new().with_fields(vec![
        field(
            TICK_TIME_KEY,
            "When the tick was scheduled",
            FieldType::Datetime,
        ),
        field(EMIT_TIME_KEY, "When the tick was sent", FieldType::Datetime),
        field(
            SEQ_KEY,
            "Tick number since the plugin started, from 1",
            FieldType::U64,
        ),
        field(
            MISSED_KEY,
            "Ticks dropped since the previous one",
            FieldType::U32,
        ),
        field(LABEL_KEY, "The node's label launch arg", FieldType::String),
    ])
}

pub(crate) fn payload(tick: &Tick, label: &str) -> PluginPayload {
    let field = |key: &str, value: Value| Field {
        key: key.into(),
        value: Some(FieldValue { value: Some(value) }),
    };
    PluginPayload {
        fields: vec![
            field(TICK_TIME_KEY, Value::DateTime(timestamp(tick.tick_time))),
            field(EMIT_TIME_KEY, Value::DateTime(timestamp(tick.emit_time))),
            field(SEQ_KEY, Value::U64(tick.seq)),
            field(MISSED_KEY, Value::U32(tick.missed)),
            field(LABEL_KEY, Value::String(label.into())),
        ],
    }
}

fn timestamp(t: Timestamp) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: t.as_second(),
        nanos: t.subsec_nanosecond(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_every_schema_field_in_order() {
        let t: Timestamp = "2026-01-01T00:00:01.5Z".parse().unwrap();
        let tick = Tick {
            tick_time: t,
            emit_time: t,
            seq: 7,
            missed: 2,
        };
        let payload = payload(&tick, "a");
        let schema: flwrs_plugin::schema::payload::SchemaDefinition = build_schema().into();
        let keys: Vec<&str> = payload.fields.iter().map(|f| f.key.as_str()).collect();
        let declared: Vec<&str> = schema.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, declared);
        assert_eq!(
            payload.fields[0].value.as_ref().unwrap().value,
            Some(Value::DateTime(pbjson_types::Timestamp {
                seconds: 1_767_225_601,
                nanos: 500_000_000
            }))
        );
    }
}
