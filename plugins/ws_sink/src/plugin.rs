//! The SDK's `Sink`: each event becomes one WebSocket message, acknowledged
//! once it's written (`ack: write`) or once the server replied (`ack: reply`).

use crate::config::{MessageType, Settings, within};
use crate::conn::{Failure, Pool, new_id};
use crate::json_path;
use flwrs_plugin::plugin::core::{InboundEvent, InitializeRequest, SchemaDefinition};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SinkError};
use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::sink::plugin::Sink;
use flwrs_types::value::Value;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The input schema, as packaged.
pub const IN_SCHEMA: &str = include_str!("../schema/in.json");

pub fn in_schema() -> SchemaDefinition {
    flwrs_plugin::schema::rules::parse_json(IN_SCHEMA)
        .expect("schema/in.json is valid (tested)")
        .into()
}

/// What an event asks for, checked.
#[derive(Debug, PartialEq)]
pub struct Outgoing {
    pub url: String,
    pub message: Message,
    /// ack reply: the id the reply must carry.
    pub id: Option<String>,
}

pub struct WsSink {
    settings: Arc<Settings>,
    pool: Option<Arc<Pool>>,
}

impl WsSink {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings: Arc::new(settings),
            pool: None,
        }
    }

    /// The message for an event, or why it can never be sent.
    pub fn outgoing(&self, payload: &PluginPayload) -> Result<Outgoing, String> {
        let s = &self.settings;
        let Value::Object(fields) = Value::from_payload(payload) else {
            unreachable!("a payload is an object")
        };
        let (mut url, mut data, mut kind, mut id) = (None, vec![], s.message_type, None);
        for (key, value) in fields {
            match (key.as_str(), value) {
                (_, Value::Null) => {}
                ("url", Value::String(u)) if u.is_empty() => {}
                ("url", Value::String(u)) => url = Some(u),
                ("data", Value::Bytes(b)) => data = b,
                ("data", Value::String(t)) => data = t.into_bytes(),
                ("message_type", Value::String(t)) => {
                    kind = match t.as_str() {
                        "text" => MessageType::Text,
                        "binary" => MessageType::Binary,
                        "" => kind,
                        other => {
                            return Err(format!(
                                "message_type must be text or binary, not {other:?}"
                            ));
                        }
                    }
                }
                ("correlation_id", Value::String(c)) if !c.is_empty() => id = Some(c),
                ("correlation_id", Value::String(_)) => {}
                (k @ ("url" | "data" | "message_type" | "correlation_id"), v) => {
                    return Err(format!("{k} has the wrong type ({})", v.type_name()));
                }
                _ => {}
            }
        }
        let url = match url {
            None => s.url.clone(),
            Some(u) if u == s.url => u,
            Some(u) if s.allowed_urls.is_empty() => {
                return Err(format!(
                    "the event names url {u:?}, but events may name their url only within allowed_urls, and none are set"
                ));
            }
            Some(u) => {
                crate::config::check_url(&u)?;
                if !s.allowed_urls.iter().any(|p| within(p, &u)) {
                    return Err(format!("url {u:?} isn't within allowed_urls"));
                }
                u
            }
        };
        let id = match &s.reply {
            None => None,
            Some(reply) => {
                let id = id.unwrap_or_else(new_id);
                match &reply.request_id_path {
                    Some(path) => {
                        let mut json: serde_json::Value =
                            serde_json::from_slice(&data).map_err(|e| {
                                format!(
                                    "ack reply puts the id into the data, which must be JSON: {e}"
                                )
                            })?;
                        json_path::set(&mut json, path, serde_json::Value::String(id.clone()))
                            .map_err(|e| format!("request_id_path: {e}"))?;
                        data = serde_json::to_vec(&json).expect("JSON serializes");
                    }
                    // The data carries the id already (the event's correlation_id).
                    None if payload_has_id(payload) => {}
                    None => {
                        return Err(
                            "ack reply without request_id_path needs the event's correlation_id"
                                .into(),
                        );
                    }
                }
                Some(id)
            }
        };
        if data.len() > s.max_message_bytes {
            return Err(format!(
                "the message is too large: {} bytes, max_message_bytes is {}",
                data.len(),
                s.max_message_bytes
            ));
        }
        let message = match kind {
            MessageType::Binary => Message::Binary(data.into()),
            MessageType::Text => {
                let text = String::from_utf8(data)
                    .map_err(|_| "a text message must be UTF-8".to_string())?;
                Message::Text(text.into())
            }
        };
        Ok(Outgoing { url, message, id })
    }
}

fn payload_has_id(payload: &PluginPayload) -> bool {
    matches!(Value::from_payload(payload), Value::Object(fields)
        if fields.iter().any(|(k, v)| k == "correlation_id" && matches!(v, Value::String(s) if !s.is_empty())))
}

impl Sink for WsSink {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
    ) -> Result<InitializeRequest, InitializeError> {
        if self.pool.is_none() {
            self.pool = Some(Pool::start(self.settings.clone()));
        }
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(VERSION.to_string())
            .with_in_schema(in_schema()))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        if let Some(pool) = &self.pool {
            pool.close_all();
        }
        Ok(())
    }

    fn version(&self) -> String {
        VERSION.to_string()
    }

    async fn consume_event(&self, event: InboundEvent) -> Result<(), SinkError> {
        let failure = |Failure { message, retryable }| {
            if retryable {
                SinkError::retryable(message)
            } else {
                SinkError::new(message)
            }
        };
        let outgoing = self.outgoing(&event.payload).map_err(SinkError::new)?;
        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| SinkError::retryable("not started yet"))?;
        let conn = pool.get(&outgoing.url).map_err(failure)?;
        let reply = self.settings.reply.as_ref().zip(outgoing.id);
        conn.send(outgoing.message, reply).await.map_err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Args;
    use clap::Parser;

    fn sink(extra: &[&str]) -> WsSink {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed.json");
        std::fs::write(&allowed, r#"["ws://other/api/"]"#).unwrap();
        let allowed = allowed.to_string_lossy().into_owned();
        let mut argv = vec![
            "s",
            "--id",
            "n",
            "--port",
            "1",
            "--url",
            "ws://main/x",
            "--allowed-urls",
            &allowed,
        ];
        argv.extend_from_slice(extra);
        WsSink::new(
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
        let s = sink(&[]);
        let o = s
            .outgoing(&payload(vec![("data", Value::String("hi".into()))]))
            .unwrap();
        assert_eq!(
            (o.url.as_str(), o.message, o.id),
            ("ws://main/x", Message::Text("hi".into()), None)
        );
        let o = s
            .outgoing(&payload(vec![
                ("url", Value::String("ws://other/api/7".into())),
                ("data", Value::Bytes(vec![0xff])),
                ("message_type", Value::String("binary".into())),
            ]))
            .unwrap();
        assert_eq!(o.message, Message::Binary(vec![0xff].into()));
        let bad_text = payload(vec![("data", Value::Bytes(vec![0xff]))]);
        assert!(s.outgoing(&bad_text).unwrap_err().contains("UTF-8"));
        for url in ["ws://other/apix", "ws://evil/api/", "http://other/api/"] {
            assert!(
                s.outgoing(&payload(vec![("url", Value::String(url.into()))]))
                    .is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn reply_ids() {
        let s = sink(&[
            "--ack",
            "reply",
            "--reply-id-path",
            "id",
            "--request-id-path",
            "id",
        ]);
        let o = s
            .outgoing(&payload(vec![("data", Value::String(r#"{"x":1}"#.into()))]))
            .unwrap();
        let id = o.id.unwrap();
        let Message::Text(text) = o.message else {
            panic!()
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({"x": 1, "id": id})
        );
        let o = s
            .outgoing(&payload(vec![
                ("data", Value::String(r#"{"x":1}"#.into())),
                ("correlation_id", Value::String("c7".into())),
            ]))
            .unwrap();
        assert_eq!(o.id.as_deref(), Some("c7"));
        assert!(
            s.outgoing(&payload(vec![("data", Value::String("not json".into()))]))
                .unwrap_err()
                .contains("JSON")
        );
        let s = sink(&["--ack", "reply", "--reply-id-path", "id"]);
        assert!(
            s.outgoing(&payload(vec![("data", Value::String("{}".into()))]))
                .unwrap_err()
                .contains("correlation_id")
        );
    }
}
