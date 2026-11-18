//! The push sinks' shared settings (a clap struct they flatten, checked with
//! every problem reported), their launch-args schema properties, the shared
//! in schema and how an event becomes a [`Message`].

use super::{AckMode, Config, Message, SlowPolicy, check_patterns};
use clap::{Args, ValueEnum};
use flwrs_plugin::cli::read_json;
use flwrs_plugin::schema::payload::field_type::Enum as FieldType;
use flwrs_plugin::schema::payload::{FieldDefinition, PluginPayload, SchemaDefinition};
use flwrs_types::value::Value;
use std::path::PathBuf;
use std::time::Duration;

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckModeArg {
    #[value(name = "delivered")]
    Delivered,
    #[value(name = "buffered")]
    Buffered,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowPolicyArg {
    #[value(name = "disconnect")]
    Disconnect,
    #[value(name = "skip")]
    Skip,
}

#[derive(Args, Debug, Clone)]
pub struct BroadcastArgs {
    /// When an event is acked: once a subscriber's connection took it, or once it's in the ring
    #[arg(long, value_enum, default_value = "delivered")]
    pub ack_mode: AckModeArg,

    /// delivered: how long an event waits for a subscriber, in ms (0: as long as it takes)
    #[arg(long, default_value_t = 0)]
    pub delivery_timeout_ms: u64,

    /// Events kept per channel for replay
    #[arg(long, default_value_t = 1000)]
    pub replay_events: usize,

    /// Bytes kept per channel for replay
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub replay_bytes: usize,

    /// A subscriber that falls behind the ring: disconnected, or on from the oldest after a gap
    #[arg(long, value_enum, default_value = "disconnect")]
    pub slow_policy: SlowPolicyArg,

    /// JSON file: the channel name patterns events and subscribers may use (* within a / segment, ** across)
    #[arg(long)]
    pub allowed_channels: Option<PathBuf>,

    /// The channel of events that name none
    #[arg(long, default_value = "default")]
    pub default_channel: String,

    #[arg(long, default_value_t = 256)]
    pub max_channels: usize,

    #[arg(long, default_value_t = 10_000)]
    pub max_subscribers: usize,

    #[arg(long, default_value_t = 1_000)]
    pub max_subscribers_per_channel: usize,
}

#[derive(Debug, Clone)]
pub struct BroadcastSettings {
    pub config: Config,
    pub ack_mode: AckMode,
}

impl BroadcastArgs {
    pub fn settings(&self) -> Result<BroadcastSettings, Vec<String>> {
        let mut problems = vec![];
        let allowed_channels: Vec<String> = self
            .allowed_channels
            .as_deref()
            .and_then(|p| {
                read_json(p)
                    .map_err(|e| problems.push(format!("--allowed-channels {e}")))
                    .ok()
            })
            .unwrap_or_default();
        problems.extend(check_patterns(&allowed_channels));
        for (name, v) in [
            ("replay_events", self.replay_events),
            ("replay_bytes", self.replay_bytes),
            ("max_channels", self.max_channels),
            ("max_subscribers", self.max_subscribers),
            (
                "max_subscribers_per_channel",
                self.max_subscribers_per_channel,
            ),
        ] {
            if v == 0 {
                problems.push(format!("{name} must be at least 1"));
            }
        }
        let config = Config {
            max_events: self.replay_events,
            max_bytes: self.replay_bytes,
            max_channels: self.max_channels,
            max_subscribers: self.max_subscribers,
            max_subscribers_per_channel: self.max_subscribers_per_channel,
            slow_policy: match self.slow_policy {
                SlowPolicyArg::Disconnect => SlowPolicy::Disconnect,
                SlowPolicyArg::Skip => SlowPolicy::Skip,
            },
            allowed_channels,
            default_channel: self.default_channel.clone(),
            delivery_timeout: (self.delivery_timeout_ms > 0)
                .then(|| Duration::from_millis(self.delivery_timeout_ms)),
        };
        // The default channel must itself be allowed.
        let probe = super::Broadcaster::with_epoch(config.clone(), String::new());
        if let Err(e) = probe.channel_name(None) {
            problems.push(format!("default_channel: {e}"));
        }
        if problems.is_empty() {
            Ok(BroadcastSettings {
                config,
                ack_mode: match self.ack_mode {
                    AckModeArg::Delivered => AckMode::Delivered,
                    AckModeArg::Buffered => AckMode::Buffered,
                },
            })
        } else {
            Err(problems)
        }
    }
}

/// The launch-args schema properties for [`BroadcastArgs`].
pub fn schema_properties() -> serde_json::Value {
    let group = |order: u32, advanced: bool, mut v: serde_json::Value| {
        let o = v.as_object_mut().expect("an object");
        o.insert("x-flwrs-group".into(), "Broadcast".into());
        o.insert("x-flwrs-order".into(), order.into());
        if advanced {
            o.insert("x-flwrs-advanced".into(), true.into());
        }
        v
    };
    serde_json::json!({
        "ack_mode": group(50, false, serde_json::json!({
            "title": "Ack", "description": "delivered: an event is acked once a subscriber's connection took it; while nobody does, it waits (unacked, so the rest wait in the node's queue, durable if it is). buffered: once it's in the channel's replay ring (faster; with nobody subscribed it's only kept for replay).",
            "enum": ["delivered", "buffered"], "default": "delivered" })),
        "delivery_timeout_ms": group(51, true, serde_json::json!({
            "title": "Delivery timeout (ms)", "description": "delivered: how long an event waits for a subscriber before it's refused (retryable: the hub delivers it again, up to the node's delivery.max_attempts). 0: as long as it takes.",
            "type": "integer", "minimum": 0, "default": 0 })),
        "replay_events": group(52, false, serde_json::json!({
            "title": "Replay events", "description": "Events kept per channel, for subscribers resuming from an id and for slow readers.",
            "type": "integer", "minimum": 1, "default": 1000 })),
        "replay_bytes": group(53, false, serde_json::json!({
            "title": "Replay bytes", "description": "Bytes of data kept per channel; a larger event is refused.",
            "type": "integer", "minimum": 1, "default": 8_388_608 })),
        "slow_policy": group(54, false, serde_json::json!({
            "title": "Slow subscribers", "description": "A subscriber that falls behind the replay ring: disconnect it, or skip it ahead to the oldest kept event after a gap marker.",
            "enum": ["disconnect", "skip"], "default": "disconnect" })),
        "allowed_channels": group(55, false, serde_json::json!({
            "title": "Allowed channels", "description": "The channel names events and subscribers may use, as patterns (* within a /-separated segment, ** across). Empty: any name.",
            "type": "array", "items": { "type": "string", "minLength": 1 }, "x-flwrs-launch": { "via": "file" } })),
        "default_channel": group(56, false, serde_json::json!({
            "title": "Default channel", "description": "The channel of events without one.",
            "type": "string", "minLength": 1, "default": "default" })),
        "max_channels": group(57, true, serde_json::json!({ "title": "Max channels", "type": "integer", "minimum": 1, "default": 256 })),
        "max_subscribers": group(58, true, serde_json::json!({ "title": "Max subscribers", "description": "In all (waiting long polls included); more are refused with 503.", "type": "integer", "minimum": 1, "default": 10_000 })),
        "max_subscribers_per_channel": group(59, true, serde_json::json!({ "title": "Max subscribers per channel", "type": "integer", "minimum": 1, "default": 1000 })),
    })
}

fn field(key: &str, t: FieldType, description: &str) -> FieldDefinition {
    FieldDefinition {
        key: key.into(),
        r#type: t as i32,
        description: description.into(),
        ..Default::default()
    }
}

/// The push sinks' in schema.
pub fn in_schema() -> SchemaDefinition {
    SchemaDefinition {
        fields: vec![
            field(
                "channel",
                FieldType::String,
                "The channel to broadcast on (allowed_channels); empty: default_channel",
            ),
            field(
                "event",
                FieldType::String,
                "The event type (SSE event:, the WebSocket envelope's event)",
            ),
            field("data", FieldType::Bytes, "The event's data"),
            field(
                "content_type",
                FieldType::String,
                "The data's media type (e.g. application/json)",
            ),
        ],
    }
}

/// An event's channel (if it names one) and message.
pub fn message_of(payload: &PluginPayload) -> (Option<String>, Message) {
    let mut channel = None;
    let mut message = Message {
        event: None,
        content_type: None,
        data: vec![],
    };
    if let Value::Object(fields) = Value::from_payload(payload) {
        for (key, value) in fields {
            let text = match &value {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            match key.as_str() {
                "channel" => channel = text,
                "event" => message.event = text,
                "content_type" => message.content_type = text,
                "data" => {
                    message.data = match value {
                        Value::Bytes(b) => b,
                        Value::String(s) => s.into_bytes(),
                        _ => vec![],
                    }
                }
                _ => {}
            }
        }
    }
    (channel, message)
}
