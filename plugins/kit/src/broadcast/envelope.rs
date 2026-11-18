//! An event as JSON (long-poll answers, SSE `json` format, the WebSocket
//! push sink's `envelope` frames).

use super::Stored;
use base64::Engine;
use serde_json::{Value, json};

/// The data as text, or base64 when it isn't UTF-8.
pub fn text_of(data: &[u8]) -> (String, bool) {
    match std::str::from_utf8(data) {
        Ok(t) => (t.to_string(), false),
        Err(_) => (base64::engine::general_purpose::STANDARD.encode(data), true),
    }
}

/// An event as JSON: in long-poll answers and SSE `json` format.
pub fn envelope(s: &Stored) -> Value {
    let (data, base64) = text_of(&s.message.data);
    let mut v = json!({ "id": s.id, "data": data });
    if let Some(e) = &s.message.event {
        v["event"] = e.clone().into();
    }
    if let Some(ct) = &s.message.content_type {
        v["content_type"] = ct.clone().into();
    }
    if base64 {
        v["encoding"] = "base64".into();
    }
    v
}
