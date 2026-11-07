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
/// Maximum length of a remote node's `remote_id`.
pub(crate) const MAX_REMOTE_ID_LEN: usize = 128;

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
    /// Remote nodes only: the id the remote plugin is started with (`--id`)
    /// and sends in `Initialize`. Unlike the node id it survives export and
    /// import, so a remote plugin keeps working after its scene moves to
    /// another server. Not unique: the token tells nodes with the same
    /// `remote_id` apart. Defaults to the node id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// Free-form launch arguments, shaped by the plugin's manifest (local nodes only).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub launch_args: Value,
    /// Remote nodes only: `token` (pre-shared, the default) or `mtls` (a
    /// client certificate). Unset = the scene's `defaults.remote_auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RemoteAuth>,
    /// Remote nodes with `token` auth only: whether they must connect to the
    /// TLS listener. Unset = the scene's default, else true when the hub has
    /// a TLS listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_tls: Option<bool>,
    /// Remote nodes with `mtls` auth only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<NodeTls>,
}

/// A remote node's client-certificate settings.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeTls {
    /// Pins the plugin's certificate: its SHA-256 fingerprint (hex, colons
    /// allowed). Unset = the certificate must chain to `[hub.tls] client_ca`
    /// and name the node's `remote_id` (SAN or CN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_sha256: Option<String>,
}

/// How the hub checks a remote node's plugin, all defaults applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EffectiveAuth {
    Token { require_tls: bool },
    /// `pin`: a normalised (lower-case hex, no colons) SHA-256 fingerprint.
    Mtls { pin: Option<String> },
}

/// A SHA-256 fingerprint in hex (colons and case ignored), normalised.
pub(crate) fn normalize_fingerprint(pin: &str) -> Option<String> {
    let hex: String = pin.chars().filter(|c| *c != ':').collect::<String>().to_ascii_lowercase();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryConfig>,
}

/// Redelivery of events to a plugin that acknowledges them (capability ACKS).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryConfig {
    /// Deliveries of one event (the first included) before it's dead-lettered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
    /// Wait before redelivering an event the plugin rejected with retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_delay_ms: Option<u64>,
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
    /// Keep queued events on disk until the plugin acknowledges them, across
    /// hub restarts. `None` = the scene's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
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

/// How a remote node's plugin authenticates.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteAuth {
    /// The node's pre-shared token (`config.token`).
    Token,
    /// A client certificate on the TLS listener.
    Mtls,
}

/// Defaults a scene sets for its nodes (`scenes.defaults`). Every key is
/// optional, and a node's own setting wins.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SceneDefaults {
    /// Keep queued events on disk until acknowledged (`runtime.queue.durable`).
    /// Default: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
    /// How remote nodes authenticate (a node's `auth`). Default: `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_auth: Option<RemoteAuth>,
    /// Whether remote nodes using a token must connect over TLS (a node's
    /// `require_tls`). Default: true when the hub has a TLS listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_tls: Option<bool>,
}

impl SceneDefaults {
    pub(crate) fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("defaults: {e}"))
    }
}

/// Upper bound for `runtime.delivery.max_attempts`.
pub(crate) const MAX_DELIVERY_ATTEMPTS: u64 = 1000;

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
        if let Some(delivery) = &self.delivery {
            positive("delivery.max_attempts", delivery.max_attempts);
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
        if let Some(n) = self.delivery.as_ref().and_then(|d| d.max_attempts)
            && n > MAX_DELIVERY_ATTEMPTS
        {
            errors.push(format!(
                "{prefix}.delivery.max_attempts can't exceed {MAX_DELIVERY_ATTEMPTS}"
            ));
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
    pub(crate) fn validate(&self, plugin: &PluginRules, defaults: &SceneDefaults) -> Vec<String> {
        let mut errors = self.validate_own();
        if let Some(missing) = self.missing_token(defaults) {
            errors.push(missing);
        }
        if let Some(unsupported) = self.unsupported() {
            errors.push(unsupported);
        }
        if self.launch == LaunchMode::Local {
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
        errors
    }

    /// The checks that don't need the plugin, and allow a remote node's token
    /// to be missing: what a scene import requires (the plugin may not be
    /// installed, and exports leave tokens out by default).
    pub(crate) fn validate_own(&self) -> Vec<String> {
        let mut errors = self.runtime.validate("config.runtime");
        match self.launch {
            LaunchMode::Local => {
                if self.token.is_some() {
                    errors.push(
                        "config.token is only for remote nodes; local plugins get a fresh token per launch".into(),
                    );
                }
                if self.remote_id.is_some() {
                    errors.push(
                        "config.remote_id is only for remote nodes; a local plugin is started with its node id".into(),
                    );
                }
                if self.auth.is_some() || self.require_tls.is_some() || self.tls.is_some() {
                    errors.push(
                        "config.auth, config.require_tls and config.tls are only for remote nodes; local plugins use a per-launch token over loopback".into(),
                    );
                }
            }
            LaunchMode::Remote => {
                if let Some(t) = &self.token
                    && t.chars().count() < MIN_TOKEN_LEN
                {
                    errors.push(format!(
                        "config.token must be at least {MIN_TOKEN_LEN} characters"
                    ));
                }
                if let Some(id) = &self.remote_id
                    && !is_valid_remote_id(id)
                {
                    errors.push(format!(
                        "config.remote_id must be 1 to {MAX_REMOTE_ID_LEN} characters of letters, digits, '.', '_', ':' and '-'"
                    ));
                }
                if !self.launch_args.is_null() {
                    errors.push(
                        "config.launch_args is only used for local nodes; a remote plugin gets its arguments from whoever starts it".into(),
                    );
                }
                if let Some(pin) = self.tls.as_ref().and_then(|t| t.client_cert_sha256.as_ref())
                    && normalize_fingerprint(pin).is_none()
                {
                    errors.push(
                        "config.tls.client_cert_sha256 must be a SHA-256 fingerprint: 64 hex digits (colons allowed)".into(),
                    );
                }
            }
        }
        errors
    }

    /// The problem if this is a remote node with token auth but no token.
    pub(crate) fn missing_token(&self, defaults: &SceneDefaults) -> Option<String> {
        (self.launch == LaunchMode::Remote
            && self.remote_auth(defaults) == RemoteAuth::Token
            && self.token.is_none())
        .then(|| "config.token is required for remote nodes with token auth".to_string())
    }

    /// The problem if this server can't run the node: a remote node on a
    /// server built without remote support (the `remote` feature).
    pub(crate) fn unsupported(&self) -> Option<String> {
        (cfg!(not(feature = "remote")) && self.launch == LaunchMode::Remote).then(|| {
            "remote nodes aren't supported: this server was built without the remote feature"
                .to_string()
        })
    }

    pub(crate) fn remote_auth(&self, defaults: &SceneDefaults) -> RemoteAuth {
        self.auth
            .or(defaults.remote_auth)
            .unwrap_or(RemoteAuth::Token)
    }

    /// How the hub checks this remote node's plugin. `tls_available`: the
    /// hub has a TLS listener (then token auth requires TLS by default).
    pub(crate) fn effective_auth(
        &self,
        defaults: &SceneDefaults,
        tls_available: bool,
    ) -> EffectiveAuth {
        match self.remote_auth(defaults) {
            RemoteAuth::Token => EffectiveAuth::Token {
                require_tls: self
                    .require_tls
                    .or(defaults.require_tls)
                    .unwrap_or(tls_available),
            },
            RemoteAuth::Mtls => EffectiveAuth::Mtls {
                pin: self
                    .tls
                    .as_ref()
                    .and_then(|t| t.client_cert_sha256.as_deref())
                    .and_then(normalize_fingerprint),
            },
        }
    }

    /// The id the node's plugin sends in `Initialize`: the `remote_id` of a
    /// remote node, else the node id.
    pub(crate) fn instance_id<'a>(&'a self, node_id: &'a str) -> &'a str {
        match (self.launch, &self.remote_id) {
            (LaunchMode::Remote, Some(remote_id)) => remote_id,
            _ => node_id,
        }
    }

    /// The config as JSON, without the token (for API responses).
    pub(crate) fn redacted(&self) -> Value {
        let mut copy = self.clone();
        copy.token = None;
        serde_json::to_value(copy).expect("config serializes")
    }
}

fn is_valid_remote_id(id: &str) -> bool {
    (1..=MAX_REMOTE_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remote_auth_follows_the_node_then_the_scene_then_the_listener() {
        let parse = |v: Value| NodeConfig::parse(&v).unwrap();
        let none = SceneDefaults::default();
        let mtls_scene = SceneDefaults {
            remote_auth: Some(RemoteAuth::Mtls),
            require_tls: Some(false),
            ..Default::default()
        };
        let token_node = parse(json!({"launch": "remote", "token": "a-long-enough-token"}));
        // Token nodes require TLS once the hub has a TLS listener.
        assert_eq!(
            token_node.effective_auth(&none, true),
            EffectiveAuth::Token { require_tls: true }
        );
        assert_eq!(
            token_node.effective_auth(&none, false),
            EffectiveAuth::Token { require_tls: false }
        );
        // The scene says mtls; a node can say token, and the scene's
        // require_tls applies to it.
        assert_eq!(
            token_node.effective_auth(&mtls_scene, true),
            EffectiveAuth::Mtls { pin: None }
        );
        let back_to_token = parse(json!({"launch": "remote", "auth": "token", "token": "a-long-enough-token"}));
        assert_eq!(
            back_to_token.effective_auth(&mtls_scene, true),
            EffectiveAuth::Token { require_tls: false }
        );
        // mtls needs no token; token auth does.
        let mtls = parse(json!({"launch": "remote", "auth": "mtls",
            "tls": {"client_cert_sha256": "AB:".repeat(31) + "AB"}}));
        assert_eq!(mtls.missing_token(&none), None);
        assert!(parse(json!({"launch": "remote"})).missing_token(&none).is_some());
        assert!(parse(json!({"launch": "remote"})).missing_token(&mtls_scene).is_none());
        assert_eq!(
            mtls.effective_auth(&none, true),
            EffectiveAuth::Mtls {
                pin: Some("ab".repeat(32))
            }
        );
    }

    #[test]
    fn remote_auth_settings_are_checked() {
        let errors = |v: Value| NodeConfig::parse(&v).unwrap().validate_own();
        assert!(errors(json!({"auth": "mtls"}))[0].contains("only for remote nodes"));
        assert!(errors(json!({"require_tls": true}))[0].contains("only for remote nodes"));
        assert!(
            errors(json!({"launch": "remote", "tls": {"client_cert_sha256": "nope"}}))[0]
                .contains("64 hex digits")
        );
        assert!(NodeConfig::parse(&json!({"launch": "remote", "auth": "password"})).is_err());
        assert_eq!(normalize_fingerprint(&"0F".repeat(32)), Some("0f".repeat(32)));
        assert_eq!(normalize_fingerprint("0f"), None);
    }

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
        assert!(cfg.validate(&no_schema(), &SceneDefaults::default()).is_empty());
        assert_eq!(
            NodeConfig::parse(&Value::Null).unwrap(),
            NodeConfig::default()
        );
    }

    #[cfg(feature = "remote")] // creates remote nodes
    #[test]
    fn remote_id_rules_and_the_instance_id() {
        let remote = |extra: serde_json::Value| {
            let mut config = json!({"launch": "remote", "token": "a-long-enough-shared-secret"});
            config
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            NodeConfig::parse(&config).unwrap()
        };
        let cfg = remote(json!({"remote_id": "plant-7:sensor_gw.1"}));
        assert!(cfg.validate(&no_schema(), &SceneDefaults::default()).is_empty());
        assert_eq!(cfg.instance_id("node-1"), "plant-7:sensor_gw.1");
        assert_eq!(
            remote(json!({})).instance_id("node-1"),
            "node-1",
            "defaults to the node id"
        );
        for bad in [
            "",
            "has space",
            "ünïcode",
            &"x".repeat(MAX_REMOTE_ID_LEN + 1),
        ] {
            let errors = remote(json!({"remote_id": bad})).validate(&no_schema(), &SceneDefaults::default());
            assert!(
                errors[0].contains("config.remote_id must be"),
                "{bad:?}: {errors:?}"
            );
        }
        let local = NodeConfig::parse(&json!({"remote_id": "x"})).unwrap();
        assert!(local.validate(&no_schema(), &SceneDefaults::default())[0].contains("only for remote nodes"));
        assert_eq!(local.instance_id("node-1"), "node-1");

        // An import accepts a remote node without its token; enabling doesn't.
        let tokenless = NodeConfig::parse(&json!({"launch": "remote"})).unwrap();
        assert!(tokenless.validate_own().is_empty());
        assert_eq!(
            tokenless.validate(&no_schema(), &SceneDefaults::default()),
            ["config.token is required for remote nodes with token auth"]
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
        let errors = cfg.validate(&no_schema(), &SceneDefaults::default());
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

    #[cfg(feature = "remote")] // creates remote nodes
    #[test]
    fn launch_mode_rules() {
        let local_with_token = NodeConfig::parse(&json!({"token": "x".repeat(20)})).unwrap();
        assert!(local_with_token.validate(&no_schema(), &SceneDefaults::default())[0].contains("only for remote nodes"));

        let remote = |extra: Value| {
            let mut cfg = json!({"launch": "remote"});
            cfg.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            NodeConfig::parse(&cfg).unwrap().validate(&no_schema(), &SceneDefaults::default())
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
        assert!(ok.validate(&rules, &SceneDefaults::default()).is_empty());
        let bad = NodeConfig::parse(&json!({"launch_args": {"t": "x", "port": 1}})).unwrap();
        let errors = bad.validate(&rules, &SceneDefaults::default());
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
