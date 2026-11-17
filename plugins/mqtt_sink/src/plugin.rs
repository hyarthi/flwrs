//! The SDK's `Sink`: each event becomes one MQTT message, acknowledged to the
//! hub once the broker has it (QoS 1 / 2) or it's written (QoS 0).

use crate::config::{ProtocolVersion, Settings};
use crate::conn::{Connection, Failure, Message, Properties};
use crate::topic::{check_topic, matches};
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest, SchemaDefinition};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::sink::plugin::Sink;
use flwrs_types::value::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The input schema, as packaged.
pub const IN_SCHEMA: &str = include_str!("../schema/in.json");

pub fn in_schema() -> SchemaDefinition {
    flwrs_plugin::schema::rules::parse_json(IN_SCHEMA)
        .expect("schema/in.json is valid (tested)")
        .into()
}

pub struct MqttSink {
    settings: Arc<Settings>,
    connection: Option<Connection>,
    /// MQTT 5 properties on a 3.1.1 connection: logged once.
    warned_v311: AtomicBool,
}

impl MqttSink {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings: Arc::new(settings),
            connection: None,
            warned_v311: AtomicBool::new(false),
        }
    }

    /// The message for an event, or why it can never be published.
    pub fn message(&self, payload: &PluginPayload) -> Result<Message, String> {
        let s = &self.settings;
        let Value::Object(fields) = Value::from_payload(payload) else {
            unreachable!("a payload is an object")
        };
        let mut topic = None;
        let mut message = Message {
            topic: String::new(),
            payload: vec![],
            qos: s.default_qos,
            retain: false,
            properties: Properties::default(),
        };
        for (key, value) in fields {
            let wrong =
                |expected: &str| format!("{key} must be {expected}, not {}", value.type_name());
            match (key.as_str(), &value) {
                (_, Value::Null) => {}
                ("topic", Value::String(t)) if t.is_empty() => {}
                ("topic", Value::String(t)) => topic = Some(t.clone()),
                ("payload", Value::Bytes(b)) => message.payload = b.clone(),
                ("payload", Value::String(t)) => message.payload = t.clone().into_bytes(),
                ("qos", Value::U32(q)) if *q <= 2 => message.qos = *q as u8,
                ("qos", Value::U32(q)) => return Err(format!("qos must be 0, 1 or 2, not {q}")),
                ("retain", Value::Bool(r)) => message.retain = *r,
                ("content_type", Value::String(t)) => {
                    message.properties.content_type = Some(t.clone())
                }
                ("response_topic", Value::String(t)) => {
                    check_topic(t).map_err(|e| format!("response_topic: {e}"))?;
                    message.properties.response_topic = Some(t.clone());
                }
                ("correlation_data", Value::Bytes(b)) => {
                    message.properties.correlation_data = Some(b.clone())
                }
                ("user_properties", Value::Map(map)) => {
                    for (name, values) in map {
                        let Value::Array(values) = values else {
                            return Err(wrong("a map of string arrays"));
                        };
                        for v in values {
                            let Value::String(v) = v else {
                                return Err(wrong("a map of string arrays"));
                            };
                            message
                                .properties
                                .user_properties
                                .push((name.clone(), v.clone()));
                        }
                    }
                }
                ("message_expiry_s", Value::U32(e)) => {
                    message.properties.message_expiry_s = Some(*e)
                }
                ("topic" | "content_type" | "response_topic", _) => return Err(wrong("a string")),
                ("payload", _) => return Err(wrong("bytes or a string")),
                ("correlation_data", _) => return Err(wrong("bytes")),
                ("qos" | "message_expiry_s", _) => return Err(wrong("a U32")),
                ("retain", _) => return Err(wrong("a bool")),
                ("user_properties", _) => return Err(wrong("a map of string arrays")),
                _ => {}
            }
        }
        message.topic = match topic {
            Some(t) if s.allowed_topics.is_empty() => {
                return Err(format!(
                    "the event names topic {t:?}, but events may name their topic only within allowed_topics, and none are set"
                ));
            }
            Some(t) => {
                check_topic(&t)?;
                if !s.allowed_topics.iter().any(|f| matches(f, &t)) {
                    return Err(format!("topic {t:?} isn't within allowed_topics"));
                }
                t
            }
            None => s
                .default_topic
                .clone()
                .ok_or("the event names no topic, and there's no default_topic")?,
        };
        if s.version == ProtocolVersion::V311 && !message.properties.is_empty() {
            if !self.warned_v311.swap(true, Ordering::Relaxed) {
                log::warn!("MQTT 5 properties are left out over MQTT 3.1.1 (logged once)");
            }
            message.properties = Properties::default();
        }
        // The fixed header, topic and properties fit in this much (a conservative bound).
        let size =
            message.payload.len() + message.topic.len() + properties_len(&message.properties) + 32;
        if size > s.max_packet_bytes as usize {
            return Err(format!(
                "the message is too large: about {size} bytes, max_packet_bytes is {}",
                s.max_packet_bytes
            ));
        }
        Ok(message)
    }
}

fn properties_len(p: &Properties) -> usize {
    p.content_type.as_ref().map_or(0, |t| t.len() + 3)
        + p.response_topic.as_ref().map_or(0, |t| t.len() + 3)
        + p.correlation_data.as_ref().map_or(0, |d| d.len() + 3)
        + p.user_properties
            .iter()
            .map(|(k, v)| k.len() + v.len() + 5)
            .sum::<usize>()
        + p.message_expiry_s.map_or(0, |_| 5)
}

impl Sink for MqttSink {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
        // Connect at once (not with the first event), so a bad broker URL or
        // credentials show in the log right away.
        if self.connection.is_none() {
            self.connection = Some(Connection::start(self.settings.clone()));
        }
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(VERSION.to_string())
            .with_in_schema(in_schema()))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        if let Some(connection) = &self.connection {
            connection.disconnect();
        }
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError> {
        let message = self.message(&event.payload).map_err(SinkError::new)?;
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| SinkError::retryable("not connected yet"))?;
        connection
            .publish(message)
            .await
            .map_err(|Failure { message, retryable }| {
                if retryable {
                    SinkError::retryable(message)
                } else {
                    SinkError::new(message)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Args;
    use clap::Parser;
    use flwrs_types::value::Value;

    fn sink(extra: &[&str]) -> MqttSink {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed.json");
        std::fs::write(&allowed, r#"["dev/+/out"]"#).unwrap();
        let mut argv = vec!["s", "--id", "n", "--port", "1", "--broker-url", "mqtt://b"];
        argv.extend_from_slice(extra);
        let allowed = allowed.to_string_lossy().into_owned();
        if !extra.contains(&"--no-allowed") {
            argv.extend(["--allowed-topics", allowed.as_str()]);
        } else {
            argv.retain(|a| *a != "--no-allowed");
        }
        MqttSink::new(
            Args::try_parse_from(argv)
                .unwrap()
                .settings_with(None)
                .unwrap(),
        )
    }

    fn payload(fields: Vec<(&str, Value)>) -> PluginPayload {
        Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
        .into_payload()
        .unwrap()
    }

    #[test]
    fn the_schema_file_is_valid() {
        let schema = flwrs_plugin::schema::rules::parse_json(IN_SCHEMA).unwrap();
        assert!(flwrs_plugin::schema::rules::check_well_formed(&schema).is_empty());
    }

    #[test]
    fn builds_messages() {
        let s = sink(&["--default-topic", "d", "--default-qos", "0"]);
        let m = s
            .message(&payload(vec![("payload", Value::String("hi".into()))]))
            .unwrap();
        assert_eq!(
            (m.topic.as_str(), m.payload.as_slice(), m.qos),
            ("d", &b"hi"[..], 0)
        );
        let m = s
            .message(&payload(vec![
                ("topic", Value::String("dev/7/out".into())),
                ("payload", Value::Bytes(vec![1, 2])),
                ("qos", Value::U32(2)),
                ("retain", Value::Bool(true)),
                ("content_type", Value::String("application/json".into())),
                (
                    "user_properties",
                    Value::Map(
                        [(
                            "a".to_string(),
                            Value::Array(vec![
                                Value::String("1".into()),
                                Value::String("2".into()),
                            ]),
                        )]
                        .into(),
                    ),
                ),
            ]))
            .unwrap();
        assert_eq!((m.topic.as_str(), m.qos, m.retain), ("dev/7/out", 2, true));
        assert_eq!(
            m.properties.user_properties,
            [("a".into(), "1".into()), ("a".into(), "2".into())]
        );
        assert_eq!(
            m.properties.content_type.as_deref(),
            Some("application/json")
        );
    }

    #[test]
    fn guards_topics_and_sizes() {
        let s = sink(&["--max-packet-bytes", "200"]);
        let named = |t: &str| payload(vec![("topic", Value::String(t.into()))]);
        assert!(s.message(&named("dev/1/out")).is_ok());
        assert!(
            s.message(&named("dev/1/in"))
                .unwrap_err()
                .contains("allowed_topics")
        );
        assert!(
            s.message(&named("dev/+/out"))
                .unwrap_err()
                .contains("wildcards")
        );
        assert!(
            s.message(&payload(vec![]))
                .unwrap_err()
                .contains("no default_topic")
        );
        let big = payload(vec![
            ("topic", Value::String("dev/1/out".into())),
            ("payload", Value::Bytes(vec![0; 300])),
        ]);
        assert!(s.message(&big).unwrap_err().contains("too large"));
        assert!(
            s.message(&payload(vec![
                ("topic", Value::String("dev/1/out".into())),
                ("qos", Value::U32(3))
            ]))
            .is_err()
        );
        let s = sink(&["--default-topic", "d", "--no-allowed"]);
        assert!(
            s.message(&named("dev/1/out"))
                .unwrap_err()
                .contains("none are set")
        );
    }

    #[test]
    fn mqtt5_properties_are_dropped_over_311() {
        let s = sink(&["--default-topic", "d", "--protocol-version", "3.1.1"]);
        let m = s
            .message(&payload(vec![(
                "content_type",
                Value::String("text/plain".into()),
            )]))
            .unwrap();
        assert!(m.properties.is_empty());
    }
}
