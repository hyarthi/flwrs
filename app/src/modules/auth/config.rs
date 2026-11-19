//! `[http.auth]`: the secret key and session settings.

use crate::modules::duration::parse_duration;
use flwrs_core::config::{self, main_config};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// Where the key can come from when the config doesn't name one.
pub(crate) const KEY_ENV: &str = "FLWRS_SECRET_KEY";
/// The shortest key accepted (bytes). `openssl rand -hex 32` makes a good one.
pub(crate) const MIN_KEY_BYTES: usize = 16;
const MAX_KEY_BYTES: usize = 1024;
const DEFAULT_TTL: &str = "7d";
/// Sessions can't outlive this, whatever the config says.
const MAX_TTL: Duration = Duration::from_secs(365 * 86_400);

/// The section as written. Strict: an unknown key is an error.
#[derive(Deserialize, Default, Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AuthConfig {
    /// The key itself. Prefer `secret_key_file` or `FLWRS_SECRET_KEY`, so the
    /// key needn't sit in the config file.
    pub secret_key: Option<String>,
    /// A file holding the key (surrounding whitespace ignored).
    #[schema(value_type = Option<String>)]
    pub secret_key_file: Option<PathBuf>,
    /// How long a login lasts: `12h`, `7d` (default), …
    pub session_ttl: Option<String>,
    /// Other origins (`https://flwrs.example.com`) allowed to send
    /// state-changing requests with a session, e.g. when a proxy serves the
    /// UI under another name than the `Host` the server sees.
    pub allowed_origins: Vec<String>,
    /// `Secure` on the session cookie: unset = when the request came through
    /// a proxy over HTTPS (`X-Forwarded-Proto: https`).
    pub secure_cookie: Option<bool>,
}

/// The section, checked and resolved.
#[derive(Debug)]
pub(crate) struct AuthSettings {
    /// `None`: no key anywhere, loopback clients only.
    pub secret_key: Option<String>,
    pub session_ttl: Duration,
    pub allowed_origins: Vec<String>,
    pub secure_cookie: Option<bool>,
}

/// Reads and resolves `[http.auth]` (missing = defaults).
pub(crate) fn read() -> Result<AuthSettings, String> {
    let path = ["http".to_string(), "auth".to_string()];
    // Read loosely first, so a broken section is an error instead of the
    // defaults (which `read_struct` falls back to).
    let section = match config::read_struct::<serde_json::Value>(main_config(), &path) {
        None => AuthConfig::default(),
        Some(value) => serde_json::from_value(value).map_err(|e| format!("[http.auth]: {e}"))?,
    };
    section
        .resolve(std::env::var(KEY_ENV).ok())
        .map_err(|problems| format!("[http.auth]: {}", problems.join("; ")))
}

impl AuthConfig {
    /// Checks every setting and picks the key: `secret_key`, else
    /// `secret_key_file`, else the environment (`env`). Every problem is
    /// reported.
    pub(crate) fn resolve(self, env: Option<String>) -> Result<AuthSettings, Vec<String>> {
        let mut problems = Vec::new();

        let key = match (self.secret_key, &self.secret_key_file) {
            (Some(_), Some(_)) => {
                problems.push("set secret_key or secret_key_file, not both".to_string());
                None
            }
            (Some(key), None) => Some(("secret_key".to_string(), key)),
            (None, Some(file)) => match std::fs::read_to_string(file) {
                Ok(text) => Some((format!("secret_key_file {}", file.display()), text)),
                Err(e) => {
                    problems.push(format!("secret_key_file {}: {e}", file.display()));
                    None
                }
            },
            (None, None) => env.map(|key| (KEY_ENV.to_string(), key)),
        };
        let secret_key = key.and_then(|(source, key)| {
            let key = key.trim().to_string();
            if key.len() < MIN_KEY_BYTES {
                problems.push(format!(
                    "the secret key ({source}) is shorter than {MIN_KEY_BYTES} bytes; `openssl rand -hex 32` makes a good one"
                ));
                None
            } else if key.len() > MAX_KEY_BYTES {
                problems.push(format!("the secret key ({source}) is longer than {MAX_KEY_BYTES} bytes"));
                None
            } else {
                Some(key)
            }
        });

        let ttl = self.session_ttl.as_deref().unwrap_or(DEFAULT_TTL);
        let session_ttl = match parse_duration(ttl) {
            Ok(d) if d > MAX_TTL => {
                problems.push(format!("session_ttl: {ttl:?} is longer than a year"));
                MAX_TTL
            }
            Ok(d) => d,
            Err(e) => {
                problems.push(format!("session_ttl: {e}"));
                MAX_TTL
            }
        };

        let mut allowed_origins = Vec::new();
        for origin in &self.allowed_origins {
            match normalize_origin(origin) {
                Some(o) => allowed_origins.push(o),
                None => problems.push(format!(
                    "allowed_origins: {origin:?} isn't an origin (scheme://host[:port], e.g. https://flwrs.example.com)"
                )),
            }
        }

        if problems.is_empty() {
            Ok(AuthSettings {
                secret_key,
                session_ttl,
                allowed_origins,
                secure_cookie: self.secure_cookie,
            })
        } else {
            Err(problems)
        }
    }
}

/// `scheme://host[:port]`, lowercase, default ports dropped; `None` for
/// anything else (paths, queries, other schemes).
pub(crate) fn normalize_origin(origin: &str) -> Option<String> {
    let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    let (scheme, authority) = origin.split_once("://")?;
    let default_port = match scheme {
        "http" => ":80",
        "https" => ":443",
        _ => return None,
    };
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    let authority = authority.strip_suffix(default_port).unwrap_or(authority);
    Some(format!("{scheme}://{authority}"))
}
