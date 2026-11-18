//! Reading an AsyncAPI 3.0 / 3.1 document (JSON or YAML): checked with
//! `roas-asyncapi` (self-contained: external `$ref`s refused), then walked
//! as JSON into the channels this server receives on (the channels of the
//! document's `receive` operations), their addresses (the WebSocket paths)
//! and the messages they take.

use enumset::EnumSet;
use flwrs_plugin_kit::schema::{external_refs, resolve};
use roas_asyncapi::validation::{Validate, ValidationOptions};
use serde_json::Value;
use std::collections::BTreeMap;

/// A message a channel takes.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageDef {
    /// Its key in the channel's `messages`.
    pub name: String,
    /// JSON (`application/json`, `…+json`): parsed, validated, typed.
    pub json: bool,
    /// Where its payload schema is (a JSON pointer), if it has one.
    pub payload: Option<String>,
}

/// A channel this server receives on.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// Its `address`, as a path (`/devices/{id}`).
    pub address: String,
    pub params: Vec<String>,
    pub messages: Vec<MessageDef>,
}

#[derive(Debug, Clone)]
pub struct Api {
    pub doc: Value,
    pub channels: Vec<Channel>,
    pub warnings: Vec<String>,
}

/// Parses the `asyncapi` setting: a JSON or YAML string, or a JSON object.
pub fn parse(setting: &Value) -> Result<Value, String> {
    match setting {
        Value::String(text) => yaml_serde::from_str::<Value>(text)
            .map_err(|e| format!("asyncapi: neither JSON nor YAML: {e}")),
        Value::Object(_) => Ok(setting.clone()),
        _ => Err("asyncapi must be a document (an object) or its text (JSON or YAML)".into()),
    }
}

fn check_with_roas(doc: &Value, version: &str) -> Result<(), Vec<String>> {
    let options = EnumSet::only(ValidationOptions::ErrorOnExternalReference);
    let result = if version.starts_with("3.0") {
        serde_json::from_value::<roas_asyncapi::v3_0::Document>(doc.clone())
            .map_err(|e| vec![format!("not a valid AsyncAPI 3.0 document: {e}")])?
            .validate(options)
    } else {
        serde_json::from_value::<roas_asyncapi::v3_1::Document>(doc.clone())
            .map_err(|e| vec![format!("not a valid AsyncAPI 3.1 document: {e}")])?
            .validate(options)
    };
    result.map_err(|e| e.to_string().lines().map(str::to_string).collect())
}

fn is_json(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

/// Payload schema formats this source reads (JSON Schema).
fn json_schema_format(format: Option<&str>) -> bool {
    match format {
        None => true,
        Some(f) => {
            let f = f.to_ascii_lowercase();
            f.starts_with("application/schema+json")
                || f.starts_with("application/vnd.aai.asyncapi")
        }
    }
}

/// Loads the document: every problem, or its receiving channels.
pub fn load(doc: Value) -> Result<Api, Vec<String>> {
    let version = match doc.get("asyncapi").and_then(Value::as_str) {
        Some(v) if v.starts_with("3.0") || v.starts_with("3.1") => v.to_string(),
        Some(v) => {
            return Err(vec![format!(
                "AsyncAPI 3.0 or 3.1 is needed; this document is {v}"
            )]);
        }
        None => {
            return Err(vec![
                "not a valid AsyncAPI document: no \"asyncapi\" version".into(),
            ]);
        }
    };
    let mut errors = vec![];
    external_refs(&doc, "", &mut errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    check_with_roas(&doc, &version)?;
    let mut warnings = vec![];
    let default_type = doc
        .get("defaultContentType")
        .and_then(Value::as_str)
        .unwrap_or("application/json")
        .to_string();
    // The channels of `receive` operations (what clients send to this server).
    let mut wanted: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
    let operations = doc
        .get("operations")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for name in operations.keys() {
        let op_ptr = format!("/operations/{}", flwrs_plugin_kit::schema::escape(name));
        let Ok((op, _)) = resolve(&doc, &op_ptr) else {
            continue;
        };
        match op.get("action").and_then(Value::as_str) {
            Some("receive") => {}
            _ => {
                warnings.push(format!(
                    "operation {name:?} sends: ignored (a push sink sends)"
                ));
                continue;
            }
        }
        let Some(channel_ref) = op.pointer("/channel/$ref").and_then(Value::as_str) else {
            errors.push(format!(
                "operation {name:?}: its channel must be a reference (#/channels/…)"
            ));
            continue;
        };
        let channel_ptr = channel_ref.trim_start_matches('#').to_string();
        // The operation's messages (references into the channel), or all of them.
        let messages = op.get("messages").and_then(Value::as_array).map(|list| {
            list.iter()
                .filter_map(|m| m.get("$ref").and_then(Value::as_str))
                .filter_map(|r| r.rsplit('/').next().map(str::to_string))
                .collect::<Vec<_>>()
        });
        let entry = wanted.entry(channel_ptr).or_insert(Some(vec![]));
        match (entry.as_mut(), messages) {
            (Some(list), Some(more)) => list.extend(more),
            (_, None) => *entry = None,
            (None, Some(_)) => {}
        }
    }
    if wanted.is_empty() && errors.is_empty() {
        errors.push(
            "the document has no receive operations: there's nothing for clients to send".into(),
        );
    }
    let mut channels = vec![];
    for (channel_ptr, only) in wanted {
        let (channel, channel_ptr) = match resolve(&doc, &channel_ptr) {
            Ok(c) => c,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let Some(address) = channel.get("address").and_then(Value::as_str) else {
            errors.push(format!("#{channel_ptr}: a channel this server receives on needs an address (its WebSocket path)"));
            continue;
        };
        let address = if address.starts_with('/') {
            address.to_string()
        } else {
            format!("/{address}")
        };
        let mut params = vec![];
        for segment in address.split('/').skip(1) {
            if segment.starts_with('{')
                && segment.ends_with('}')
                && segment.matches('{').count() == 1
            {
                params.push(segment[1..segment.len() - 1].to_string());
            } else if segment.contains(['{', '}']) {
                errors.push(format!(
                    "{address}: a parameter must be a whole path segment"
                ));
            }
        }
        let mut messages = vec![];
        let all = channel
            .get("messages")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for name in all.keys() {
            if only.as_ref().is_some_and(|o| !o.contains(name)) {
                continue;
            }
            let msg_ptr = format!(
                "{channel_ptr}/messages/{}",
                flwrs_plugin_kit::schema::escape(name)
            );
            let (message, msg_ptr) = match resolve(&doc, &msg_ptr) {
                Ok(m) => m,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            let content_type = message
                .get("contentType")
                .and_then(Value::as_str)
                .unwrap_or(&default_type);
            let payload = match message.get("payload") {
                None => None,
                Some(p) => {
                    // A multi-format schema object: { schemaFormat, schema }.
                    if p.get("schemaFormat").is_some()
                        || p.get("schema").is_some() && p.get("type").is_none()
                    {
                        if !json_schema_format(p.get("schemaFormat").and_then(Value::as_str)) {
                            errors.push(format!(
                                "#{msg_ptr}/payload: only JSON Schema payloads are supported"
                            ));
                            continue;
                        }
                        Some(format!("{msg_ptr}/payload/schema"))
                    } else {
                        Some(format!("{msg_ptr}/payload"))
                    }
                }
            };
            messages.push(MessageDef {
                name: name.clone(),
                json: is_json(content_type),
                payload,
            });
        }
        if messages.is_empty() {
            errors.push(format!("{address}: the channel takes no messages"));
        }
        channels.push(Channel {
            address,
            params,
            messages,
        });
    }
    let mut seen = BTreeMap::new();
    for c in &channels {
        let shape: String = c
            .address
            .split('/')
            .map(|s| if s.starts_with('{') { "{}" } else { s })
            .collect::<Vec<_>>()
            .join("/");
        if let Some(other) = seen.insert(shape, c.address.clone()) {
            errors.push(format!(
                "{} and {other} differ only in their parameter names",
                c.address
            ));
        }
    }
    if errors.is_empty() {
        Ok(Api {
            doc,
            channels,
            warnings,
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const READINGS: &str = r##"
asyncapi: 3.0.0
info: { title: Readings, version: "1" }
channels:
  readings:
    address: /devices/{deviceId}
    parameters:
      deviceId: { description: The device }
    messages:
      reading: { $ref: "#/components/messages/Reading" }
      status:
        payload:
          type: object
          properties:
            kind: { type: string, const: status }
            online: { type: boolean }
          required: [kind, online]
  outgoing:
    address: /out
    messages:
      note: { payload: { type: string } }
operations:
  receiveReadings:
    action: receive
    channel: { $ref: "#/channels/readings" }
  sendNotes:
    action: send
    channel: { $ref: "#/channels/outgoing" }
components:
  messages:
    Reading:
      payload:
        type: object
        properties:
          temp: { type: number, maximum: 100 }
        required: [temp]
"##;

    #[test]
    fn loads_the_receiving_channels() {
        let api = load(parse(&Value::String(READINGS.into())).unwrap()).unwrap();
        assert_eq!(api.channels.len(), 1);
        let c = &api.channels[0];
        assert_eq!(
            (c.address.as_str(), c.params.as_slice()),
            ("/devices/{deviceId}", &["deviceId".to_string()][..])
        );
        let names: Vec<&str> = c.messages.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["reading", "status"]);
        assert!(c.messages.iter().all(|m| m.json));
        let (schema, _) = resolve(&api.doc, c.messages[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(schema["properties"]["temp"]["type"], "number");
        assert!(api.warnings[0].contains("sends"));
    }

    #[test]
    fn refuses_what_it_cant_serve() {
        let errors = |doc: Value| load(doc).unwrap_err().join(" | ");
        assert!(errors(json!({"asyncapi": "2.6.0"})).contains("3.0 or 3.1 is needed"));
        let base = |channels: Value, ops: Value| json!({"asyncapi": "3.0.0", "info": {"title": "t", "version": "1"}, "channels": channels, "operations": ops});
        let e = errors(base(
            json!({"c": {"address": "/a", "messages": {"m": {"payload": {"$ref": "other.yaml#/x"}}}}}),
            json!({}),
        ));
        assert!(e.contains("self-contained"), "{e}");
        let e = errors(base(
            json!({"c": {"address": "/a", "messages": {"m": {}}}}),
            json!({}),
        ));
        assert!(e.contains("no receive operations"), "{e}");
        let e = errors(base(
            json!({"c": {"address": "/f/{x}.json", "parameters": {"x": {}}, "messages": {"m": {}}}}),
            json!({"r": {"action": "receive", "channel": {"$ref": "#/channels/c"}}}),
        ));
        assert!(e.contains("whole path segment"), "{e}");
    }
}
