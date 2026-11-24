//! The config file's sections as the server page knows them: each one's
//! JSON Schema (from the config struct: `utoipa::ToSchema`, doc comments as
//! descriptions, the struct's `Default` as defaults), how it's parsed (the
//! struct the server itself reads it with), its own extra checks, its
//! secrets and the keys the UI may never change.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::sync::LazyLock;
use utoipa::ToSchema;

/// A section of the config file.
pub(crate) struct Section {
    /// Its dotted path, e.g. `http.auth`.
    pub key: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub schema: fn() -> Value,
    /// The section (absent: `None`) parsed with the server's own struct; the
    /// effective values as JSON.
    pub parse: fn(Option<&toml::Value>) -> Result<Value, String>,
    /// Checks beyond the struct's (e.g. `[http.auth]`'s key rules).
    pub extra: fn(Option<&toml::Value>) -> Vec<String>,
    /// JSON pointers of secret values inside the section (`*`: any key).
    pub secrets: &'static [&'static str],
    /// JSON pointers the UI can't change (`""`: the whole section).
    pub locked: &'static [&'static str],
}

impl Section {
    pub(crate) fn path(&self) -> Vec<&'static str> {
        self.key.split('.').collect()
    }
}

fn none(_: Option<&toml::Value>) -> Vec<String> {
    vec![]
}

/// The section's value parsed with `T` (absent = `T::default()`), as JSON.
fn parse<T: DeserializeOwned + Serialize + Default>(
    value: Option<&toml::Value>,
) -> Result<Value, String> {
    let parsed: T = match value {
        None => T::default(),
        Some(v) => v
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| e.message().trim().to_string())?,
    };
    serde_json::to_value(&parsed).map_err(|e| e.to_string())
}

/// `T`'s JSON Schema: referenced types under `$defs`, nullable `oneOf`s
/// collapsed to the type, and `T::default()`'s values as `default`s.
fn schema<T: ToSchema + Serialize + Default>() -> Value {
    let mut refs = vec![];
    T::schemas(&mut refs);
    let schema = T::schema();
    let order = declared_order(&schema);
    let mut root = serde_json::to_value(schema).unwrap_or(Value::Null);
    if !order.is_empty() {
        root["x-order"] = Value::from(order);
    }
    let mut defs = Map::new();
    for (name, schema) in refs {
        let order = declared_order(&schema);
        let mut v = serde_json::to_value(schema).unwrap_or(Value::Null);
        tidy(&mut v);
        if !order.is_empty() {
            v["x-order"] = Value::from(order);
        }
        defs.insert(name, v);
    }
    tidy(&mut root);
    if let Ok(defaults) = serde_json::to_value(T::default()) {
        add_defaults(&mut root, &defaults);
    }
    if !defs.is_empty() {
        root["$defs"] = Value::Object(defs);
    }
    root
}

/// An object schema's property names in their declared order (JSON objects
/// sort their keys, so the order goes alongside as `x-order`).
fn declared_order(schema: &utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>) -> Vec<String> {
    match schema {
        utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(o)) => {
            o.properties.keys().cloned().collect()
        }
        _ => vec![],
    }
}

/// `#/components/schemas/X` → `#/$defs/X`; `oneOf: [{type: null}, S]` → S.
fn tidy(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get_mut("$ref") {
                *r = r.replace("#/components/schemas/", "#/$defs/");
            }
            if let Some(Value::Array(options)) = map.get("oneOf") {
                let others: Vec<&Value> = options
                    .iter()
                    .filter(|o| o.get("type") != Some(&Value::String("null".into())))
                    .collect();
                if others.len() == 1 && options.len() == 2 {
                    let mut only = others[0].clone();
                    if let (Value::Object(o), Some(d)) = (&mut only, map.get("description")) {
                        o.entry("description").or_insert(d.clone());
                    }
                    *v = only;
                    tidy(v);
                    return;
                }
            }
            // `type: ["string", "null"]` → `type: "string"` (left out = default).
            if let Some(Value::Array(types)) = map.get("type") {
                let real: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
                if real.len() == 1 {
                    map.insert("type".into(), real[0].clone());
                }
            }
            for value in map.values_mut() {
                tidy(value);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(tidy),
        _ => {}
    }
}

/// Copies the default values into the properties' `default` (where the
/// default isn't null).
fn add_defaults(schema: &mut Value, defaults: &Value) {
    let (Some(props), Some(values)) = (
        schema.get_mut("properties").and_then(Value::as_object_mut),
        defaults.as_object(),
    ) else {
        return;
    };
    for (key, prop) in props.iter_mut() {
        match values.get(key) {
            Some(Value::Null) | None => {}
            Some(v @ Value::Object(_)) => add_defaults(prop, v),
            Some(v) => {
                if let Value::Object(p) = prop {
                    p.entry("default").or_insert(v.clone());
                }
            }
        }
    }
}

/// `[http.auth]`'s own rules (key, sizes, durations, origins).
fn auth_extra(value: Option<&toml::Value>) -> Vec<String> {
    let config: crate::modules::auth::config::AuthConfig = match value {
        None => return vec![],
        Some(v) => match v.clone().try_into() {
            Ok(c) => c,
            Err(_) => return vec![], // reported by `parse`
        },
    };
    match config.resolve(std::env::var("FLWRS_SECRET_KEY").ok()) {
        Ok(_) => vec![],
        Err(problems) => problems,
    }
}

/// `[telemetry]`: each file sink's durations and limits.
fn telemetry_extra(value: Option<&toml::Value>) -> Vec<String> {
    let config: crate::modules::telemetry::config::TelemetryConfig = match value {
        None => return vec![],
        Some(v) => match v.clone().try_into() {
            Ok(c) => c,
            Err(_) => return vec![],
        },
    };
    config
        .files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.resolve().err().map(|e| format!("file[{i}]: {e}")))
        .collect()
}

/// `[director]`: the pre-flight retry limits.
fn director_extra(value: Option<&toml::Value>) -> Vec<String> {
    let Some(Ok(config)) = value.map(|v| {
        v.clone()
            .try_into::<crate::modules::director::config::DirectorConfig>()
    }) else {
        return vec![];
    };
    match config.preflight_retry.resolve() {
        Ok(_) => vec![],
        Err(problems) => problems
            .into_iter()
            .map(|p| format!("preflight_retry.{p}"))
            .collect(),
    }
}

/// `[server]`: how the server restarts itself (from the server page), and
/// how long a restart with a changed config waits for the page to confirm it.
#[derive(serde::Deserialize, Serialize, Clone, Debug, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ServerSection {
    /// `exec`: the server restarts itself (on Unix it replaces its own
    /// process: same PID, so systemd / launchd keep tracking it; elsewhere it
    /// starts a new process and exits). `exit`: it stops with status 75 and a
    /// service manager starts it again.
    pub restart: RestartMode,
    /// After a restart with a changed config, the web UI must confirm it
    /// within this many seconds, or the previous config comes back (and the
    /// server restarts again). At least 30.
    pub confirm_timeout_s: u64,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            restart: RestartMode::Exec,
            confirm_timeout_s: 120,
        }
    }
}

#[derive(serde::Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RestartMode {
    Exec,
    Exit,
}

fn server_extra(value: Option<&toml::Value>) -> Vec<String> {
    let Some(Ok(section)) = value.map(|v| v.clone().try_into::<ServerSection>()) else {
        return vec![];
    };
    if section.confirm_timeout_s < 30 {
        vec!["confirm_timeout_s must be at least 30".into()]
    } else {
        vec![]
    }
}

pub(crate) fn server_section(doc: &toml::Table) -> ServerSection {
    doc.get("server")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default()
}

/// Every section, in the order the page shows them.
pub(crate) fn sections() -> &'static [Section] {
    static SECTIONS: LazyLock<Vec<Section>> = LazyLock::new(|| {
        use crate::modules::{director, hub, plugin, scene, telemetry};
        vec![
            Section {
                key: "http",
                title: "HTTP",
                summary: "The management API and the web UI: address, Swagger UI, config editing.",
                schema: schema::<flwrs_core::http::ServerConfig>,
                parse: parse::<flwrs_core::http::ServerConfig>,
                extra: none,
                secrets: &[],
                locked: &["/config_edit"],
            },
            Section {
                key: "http.auth",
                title: "Authentication",
                summary: "Who may use the API and the web UI. Changed only in the file (locked here).",
                schema: schema::<crate::modules::auth::config::AuthConfig>,
                parse: parse::<crate::modules::auth::config::AuthConfig>,
                extra: auth_extra,
                secrets: &["/secret_key"],
                locked: &[""],
            },
            Section {
                key: "runtime",
                title: "Runtime",
                summary: "The server's own threads.",
                schema: schema::<crate::RuntimeSection>,
                parse: parse::<crate::RuntimeSection>,
                extra: none,
                secrets: &[],
                locked: &[],
            },
            Section {
                key: "logging",
                title: "Logging",
                summary: "The server's log: level and sinks (console, rotating files, syslog).",
                schema: schema::<flwrs_core::logging::LoggerConfig>,
                parse: parse::<flwrs_core::logging::LoggerConfig>,
                extra: none,
                secrets: &[],
                locked: &[],
            },
            Section {
                key: "db.main",
                title: "Database",
                summary: "The main SQLite database file.",
                schema: schema::<flwrs_core::db::DbConfig>,
                parse: parse::<flwrs_core::db::DbConfig>,
                extra: none,
                secrets: &[],
                locked: &[],
            },
            Section {
                key: "hub",
                title: "Hub",
                summary: "Where plugins connect, and the protocol's limits; [hub.tls] for remote plugins over TLS.",
                schema: schema::<hub::config::HubConfig>,
                parse: parse::<hub::config::HubConfig>,
                extra: none,
                secrets: &[],
                locked: &[],
            },
            Section {
                key: "director",
                title: "Director",
                summary: "Running scenes: hop limit, folders, spilling to disk, plugin cores.",
                schema: schema::<director::config::DirectorConfig>,
                parse: parse::<director::config::DirectorConfig>,
                extra: director_extra,
                secrets: &[],
                locked: &[],
            },
            Section {
                key: "scenes",
                title: "Scenes",
                summary: "Upload and import limits, the live views' interval.",
                schema: schema::<scene::config::ScenesConfig>,
                parse: parse::<scene::config::ScenesConfig>,
                extra: none,
                secrets: &[],
                locked: &[],
            },
            Section {
                key: "plugins",
                title: "Plugins",
                summary: "The plugin registry: folder, uploads (locked here), limits, configurators.",
                schema: schema::<plugin::config::PluginsConfig>,
                parse: parse::<plugin::config::PluginsConfig>,
                extra: none,
                secrets: &["/admin_token"],
                locked: &["/admin_token", "/allow_upload", "/dir"],
            },
            Section {
                key: "telemetry",
                title: "Telemetry",
                summary: "The server's stats to an OpenTelemetry collector and / or local files.",
                schema: schema::<telemetry::config::TelemetryConfig>,
                parse: parse::<telemetry::config::TelemetryConfig>,
                extra: telemetry_extra,
                secrets: &["/otlp/headers/*"],
                locked: &[],
            },
            Section {
                key: "server",
                title: "Server",
                summary: "Restarts from this page: how, and how long a changed config waits to be confirmed.",
                schema: schema::<ServerSection>,
                parse: parse::<ServerSection>,
                extra: server_extra,
                secrets: &[],
                locked: &[],
            },
        ]
    });
    &SECTIONS
}

/// A section's value in a parsed file.
pub(crate) fn lookup<'a>(doc: &'a toml::Table, section: &Section) -> Option<&'a toml::Value> {
    let path = section.path();
    let mut value = doc.get(path[0])?;
    for part in &path[1..] {
        value = value.get(part)?;
    }
    Some(value)
}

/// The keys of `section` that are other sections (`auth` in `http`).
pub(crate) fn child_keys(section: &Section) -> Vec<&'static str> {
    let prefix = format!("{}.", section.key);
    sections()
        .iter()
        .filter_map(|s| s.key.strip_prefix(&prefix))
        .filter(|rest| !rest.contains('.'))
        .collect()
}
