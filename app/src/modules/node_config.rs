//! A scene node's `config` JSON: how the hub launches and supervises the
//! plugin instance. Shared by the scene service (validation), the plugin
//! registry (manifest `runtime_defaults`) and, later, the director.

use crate::modules::plugin::manifest::{self, LaunchArgsStyle};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Plugins back off up to this long between reconnect attempts (protocol rule),
/// so the hub's reconnect timeout must be longer.
pub(crate) const PLUGIN_MAX_RECONNECT_BACKOFF_MS: u64 =
    flwrs_plugin::plugin::core::MAX_RECONNECT_DELAY.as_millis() as u64;
/// Minimum length of a remote node's pre-shared token.
pub(crate) const MIN_TOKEN_LEN: usize = 16;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaunchMode {
    /// The hub starts the plugin as a child process.
    #[default]
    Local,
    /// The plugin runs elsewhere and connects with a pre-shared token.
    Remote,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeConfig {
    #[serde(default)]
    pub launch: LaunchMode,
    #[serde(default, skip_serializing_if = "RuntimeConfig::is_empty")]
    pub runtime: RuntimeConfig,
    /// Pre-shared token (remote nodes only). Write-only in the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Free-form launch arguments, shaped by the plugin's manifest (local nodes only).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub launch_args: Value,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_backoff: Option<BackoffConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_restarts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueConfig>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackoffConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_after_ms: Option<u64>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WhenFull {
    Block,
    DropNewest,
    DropOldest,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueueConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_event_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_full: Option<WhenFull>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill: Option<SpillConfig>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpillConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_max_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refill_watermark: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_batch: Option<u64>,
}

impl RuntimeConfig {
    fn is_empty(&self) -> bool {
        *self == RuntimeConfig::default()
    }

    /// Parses a `runtime` block (node config or manifest `runtime_defaults`).
    pub(crate) fn parse(value: &Value) -> Result<Self, String> {
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())
    }

    /// The node's explicit queue policy, if any.
    pub(crate) fn when_full(&self) -> Option<WhenFull> {
        self.queue.as_ref().and_then(|q| q.when_full)
    }

    /// Range and consistency checks; `prefix` labels the errors (e.g. `runtime`).
    pub(crate) fn validate(&self, prefix: &str) -> Vec<String> {
        let mut errors = vec![];
        let mut positive = |name: &str, value: Option<u64>| {
            if value == Some(0) {
                errors.push(format!("{prefix}.{name} must be at least 1"));
            }
        };
        positive("init_timeout_ms", self.init_timeout_ms);
        positive("reconnect_timeout_ms", self.reconnect_timeout_ms);
        positive("restart_delay_ms", self.restart_delay_ms);
        positive("kill_grace_ms", self.kill_grace_ms);
        positive("shutdown_grace_ms", self.shutdown_grace_ms);
        if let Some(backoff) = &self.restart_backoff {
            positive("restart_backoff.max_delay_ms", backoff.max_delay_ms);
            positive("restart_backoff.reset_after_ms", backoff.reset_after_ms);
        }
        if let Some(queue) = &self.queue {
            positive("queue.capacity", queue.capacity);
            positive("queue.max_bytes", queue.max_bytes);
            positive("queue.max_event_bytes", queue.max_event_bytes);
            positive("queue.block_timeout_ms", queue.block_timeout_ms);
            if let Some(spill) = &queue.spill {
                positive("queue.spill.memory_max_events", spill.memory_max_events);
                positive("queue.spill.memory_max_bytes", spill.memory_max_bytes);
                positive("queue.spill.disk_max_events", spill.disk_max_events);
                positive("queue.spill.disk_max_bytes", spill.disk_max_bytes);
                positive("queue.spill.write_batch", spill.write_batch);
            }
        }
        if let Some(t) = self.reconnect_timeout_ms
            && t <= PLUGIN_MAX_RECONNECT_BACKOFF_MS
        {
            errors.push(format!(
                "{prefix}.reconnect_timeout_ms must be longer than the plugins' maximum reconnect backoff ({PLUGIN_MAX_RECONNECT_BACKOFF_MS} ms)"
            ));
        }
        if let Some(m) = self.restart_backoff.as_ref().and_then(|b| b.multiplier)
            && !(m > 1.0 && m.is_finite())
        {
            errors.push(format!(
                "{prefix}.restart_backoff.multiplier must be greater than 1"
            ));
        }
        if let Some(queue) = &self.queue {
            if let (Some(event), Some(total)) = (queue.max_event_bytes, queue.max_bytes)
                && event > total
            {
                errors.push(format!(
                    "{prefix}.queue.max_event_bytes can't exceed queue.max_bytes"
                ));
            }
            if let Some(w) = queue.spill.as_ref().and_then(|s| s.refill_watermark)
                && !(w > 0.0 && w < 1.0)
            {
                errors.push(format!(
                    "{prefix}.queue.spill.refill_watermark must be between 0 and 1"
                ));
            }
        }
        errors
    }
}

/// What the node's plugin says about its configuration.
pub(crate) struct PluginRules<'a> {
    pub launch_args_schema: Option<&'a jsonschema::Validator>,
    pub launch_args_style: LaunchArgsStyle,
}

impl NodeConfig {
    /// Parses a node's `config` JSON (`null` = all defaults).
    pub(crate) fn parse(value: &Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        if !value.is_object() {
            return Err("config must be a JSON object".into());
        }
        serde_json::from_value(value.clone()).map_err(|e| format!("config: {e}"))
    }

    /// Validates the config against the hub's rules and the plugin's manifest.
    pub(crate) fn validate(&self, plugin: &PluginRules) -> Vec<String> {
        let mut errors = self.runtime.validate("config.runtime");
        match self.launch {
            LaunchMode::Local => {
                if self.token.is_some() {
                    errors.push(
                        "config.token is only for remote nodes; local plugins get a fresh token per launch".into(),
                    );
                }
                errors.extend(
                    manifest::validate_launch_args(
                        plugin.launch_args_schema,
                        plugin.launch_args_style,
                        &self.launch_args,
                    )
                    .into_iter()
                    .map(|e| format!("config.{e}")),
                );
            }
            LaunchMode::Remote => {
                match &self.token {
                    None => errors.push("config.token is required for remote nodes".into()),
                    Some(t) if t.chars().count() < MIN_TOKEN_LEN => errors.push(format!(
                        "config.token must be at least {MIN_TOKEN_LEN} characters"
                    )),
                    Some(_) => {}
                }
                if !self.launch_args.is_null() {
                    errors.push(
                        "config.launch_args is only used for local nodes; a remote plugin gets its arguments from whoever starts it".into(),
                    );
                }
            }
        }
        errors
    }

    /// The config as JSON, without the token (for API responses).
    pub(crate) fn redacted(&self) -> Value {
        let mut copy = self.clone();
        copy.token = None;
        serde_json::to_value(copy).expect("config serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_schema() -> PluginRules<'static> {
        PluginRules {
            launch_args_schema: None,
            launch_args_style: LaunchArgsStyle::Flags,
        }
    }

    #[test]
    fn full_config_parses_and_validates() {
        let cfg = NodeConfig::parse(&json!({
            "launch": "local",
            "runtime": {
                "init_timeout_ms": 10000, "reconnect_timeout_ms": 60000, "restart_delay_ms": 5000,
                "restart_backoff": {"multiplier": 2.0, "max_delay_ms": 300000, "reset_after_ms": 300000},
                "kill_grace_ms": 2000, "max_restarts": null, "shutdown_grace_ms": 10000,
                "queue": {"capacity": 1024, "max_bytes": 67108864, "max_event_bytes": 16777216,
                          "when_full": "drop_oldest", "block_timeout_ms": 30000,
                          "spill": {"enabled": true, "memory_max_events": 256, "memory_max_bytes": 8388608,
                                    "disk_max_events": 1000000, "disk_max_bytes": 1073741824,
                                    "refill_watermark": 0.25, "write_batch": 256}}
            },
            "launch_args": {"http_timeout_seconds": 60}
        }))
        .unwrap();
        assert_eq!(cfg.runtime.when_full(), Some(WhenFull::DropOldest));
        assert!(cfg.validate(&no_schema()).is_empty());
        assert_eq!(
            NodeConfig::parse(&Value::Null).unwrap(),
            NodeConfig::default()
        );
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_rejected() {
        for bad in [
            json!({"lanch": "local"}),
            json!({"runtime": {"init_timeout": 5}}),
            json!({"runtime": {"init_timeout_ms": -5}}),
            json!({"runtime": {"init_timeout_ms": 1.5}}),
            json!({"runtime": {"queue": {"when_full": "panic"}}}),
            json!({"launch": "elsewhere"}),
            json!([1]),
        ] {
            assert!(NodeConfig::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn runtime_ranges() {
        let cfg = NodeConfig::parse(&json!({"runtime": {
            "init_timeout_ms": 0, "reconnect_timeout_ms": 30000,
            "restart_backoff": {"multiplier": 1.0},
            "queue": {"max_event_bytes": 10, "max_bytes": 5, "spill": {"refill_watermark": 1.5, "write_batch": 0}}
        }}))
        .unwrap();
        let errors = cfg.validate(&no_schema());
        for fragment in [
            "runtime.init_timeout_ms must be at least 1",
            "reconnect_timeout_ms must be longer",
            "multiplier must be greater than 1",
            "max_event_bytes can't exceed",
            "refill_watermark must be between 0 and 1",
            "spill.write_batch must be at least 1",
        ] {
            assert!(
                errors.iter().any(|e| e.contains(fragment)),
                "missing {fragment:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn launch_mode_rules() {
        let local_with_token = NodeConfig::parse(&json!({"token": "x".repeat(20)})).unwrap();
        assert!(local_with_token.validate(&no_schema())[0].contains("only for remote nodes"));

        let remote = |extra: Value| {
            let mut cfg = json!({"launch": "remote"});
            cfg.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            NodeConfig::parse(&cfg).unwrap().validate(&no_schema())
        };
        assert!(remote(json!({}))[0].contains("token is required"));
        assert!(remote(json!({"token": "short"}))[0].contains("at least 16"));
        assert!(remote(json!({"token": "a-long-enough-shared-secret"})).is_empty());
        let errors =
            remote(json!({"token": "a-long-enough-shared-secret", "launch_args": {"x": 1}}));
        assert!(errors[0].contains("only used for local nodes"));
    }

    #[test]
    fn launch_args_follow_the_plugin() {
        let schema = jsonschema::validator_for(&json!({
            "type": "object", "properties": {"t": {"type": "integer"}}, "additionalProperties": false
        }))
        .unwrap();
        let rules = PluginRules {
            launch_args_schema: Some(&schema),
            launch_args_style: LaunchArgsStyle::Flags,
        };
        let ok = NodeConfig::parse(&json!({"launch_args": {"t": 1}})).unwrap();
        assert!(ok.validate(&rules).is_empty());
        let bad = NodeConfig::parse(&json!({"launch_args": {"t": "x", "port": 1}})).unwrap();
        let errors = bad.validate(&rules);
        assert!(
            errors.iter().all(|e| e.starts_with("config.launch_args")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("--port is set by the hub")),
            "{errors:?}"
        );
    }

    #[test]
    fn redaction_drops_the_token() {
        let cfg =
            NodeConfig::parse(&json!({"launch": "remote", "token": "a-long-enough-shared-secret"}))
                .unwrap();
        assert_eq!(cfg.redacted(), json!({"launch": "remote"}));
    }
}
