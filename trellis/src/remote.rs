//! Remote plugins: a plugin that runs elsewhere (a remote plugin
//! definition, manifest `"launch": "remote"`, or any plugin checked as a
//! remote node). Trellis doesn't start it: `[remote]` in `trellis.toml` gives
//! the commands that make it start or connect to trellis's hub, and its
//! configurator's endpoint (`docs/plugin-spec.md` §4.1), called as the server
//! calls it.
//!
//! ```toml
//! [remote]
//! start = ["ssh", "edge-1", "systemctl start my-plugin@${hub_host}:${hub_port}"]
//! stop = ["ssh", "edge-1", "systemctl stop my-plugin@${hub_host}:${hub_port}"]
//! host = "0.0.0.0"          # where trellis's hub listens (default 127.0.0.1)
//! port = 7401               # its port (default: a free one)
//! advertise = "10.0.0.5"    # the hub address the plugin is given (default: host, loopback for 0.0.0.0)
//! id = "edge-1-sensor"      # the remote side's instance id (remote_id), if fixed
//! token = "${env:EDGE_TOKEN}"  # its pre-shared token, if fixed (default: a fresh one per start)
//!
//! [remote.configurator]
//! url = "https://edge-1:9443/configure"
//! auth = { type = "bearer", token = "${env:CONFIG_TOKEN}" }   # or basic / mtls (cert, key: PEM files)
//! ca = "certs/edge-ca.pem"  # default: the bundled Mozilla roots
//! send_secrets = false
//! ```
//!
//! In `start` and `stop`: `${hub_host}`, `${hub_port}`, `${id}`, `${token}`
//! and `${log_level}`; the token is also in `FLWRS_TOKEN`. `start` may keep
//! running (a wrapper running the plugin, stopped when the case ends) or exit
//! once the plugin is on its way. In every string, `${env:NAME}` is the
//! environment variable `NAME` (keep secrets out of the file).

use flwrs_plugin::package::configure::{self, Configured, Invocation, RemoteResponse};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How a remote plugin is made to connect, and its configurator endpoint.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RemoteLaunch {
    /// Starts the plugin, or makes it connect.
    pub start: Vec<String>,
    /// Stops it (after each case, before a restart); optional.
    pub stop: Vec<String>,
    /// Where the fake hub listens (default `127.0.0.1`).
    pub host: Option<String>,
    /// The fake hub's port (0 = a free one).
    pub port: u16,
    /// The hub host the plugin is given (default: `host`; loopback for a wildcard).
    pub advertise: Option<String>,
    /// The instance id the plugin declares (default [`crate::INSTANCE_ID`]).
    pub id: Option<String>,
    /// The pre-shared token it authenticates with (default: fresh per start).
    pub token: Option<String>,
    pub configurator: Option<RemoteConfigurator>,
    /// Relative paths (commands' working directory, PEM files) start here.
    #[serde(skip)]
    pub dir: PathBuf,
}

/// A remote configurator endpoint, as a remote node's `config.configurator`
/// names it, with local files for the CA and the mTLS certificate and key.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfigurator {
    pub url: String,
    #[serde(default)]
    pub auth: Option<Auth>,
    /// PEM file with the endpoint's CA(s).
    #[serde(default)]
    pub ca: Option<PathBuf>,
    /// Also send the node's secret settings.
    #[serde(default)]
    pub send_secrets: bool,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Auth {
    Basic { username: String, password: Option<String> },
    Bearer { token: String },
    Mtls { cert: PathBuf, key: PathBuf },
}

impl RemoteLaunch {
    /// `${env:NAME}` in every string; relative paths against `dir`.
    pub(crate) fn resolve(mut self, dir: &Path) -> Result<Self, String> {
        let env = |s: &mut String| -> Result<(), String> {
            *s = expand_env(s)?;
            Ok(())
        };
        for s in self.start.iter_mut().chain(self.stop.iter_mut()) {
            env(s)?;
        }
        for s in [&mut self.host, &mut self.advertise, &mut self.id, &mut self.token]
            .into_iter()
            .flatten()
        {
            env(s)?;
        }
        if let Some(c) = &mut self.configurator {
            env(&mut c.url)?;
            match &mut c.auth {
                Some(Auth::Basic { username, password }) => {
                    env(username)?;
                    if let Some(p) = password {
                        env(p)?;
                    }
                }
                Some(Auth::Bearer { token }) => env(token)?,
                Some(Auth::Mtls { cert, key }) => {
                    *cert = dir.join(&*cert);
                    *key = dir.join(&*key);
                }
                None => {}
            }
            if let Some(ca) = &mut c.ca {
                *ca = dir.join(&*ca);
            }
        }
        self.dir = dir.to_path_buf();
        Ok(self)
    }

    /// Where the fake hub listens.
    pub(crate) fn bind_host(&self) -> &str {
        self.host.as_deref().unwrap_or("127.0.0.1")
    }

    /// The hub host the plugin is given.
    pub(crate) fn hub_host(&self) -> String {
        match &self.advertise {
            Some(a) => a.clone(),
            None => match self.bind_host() {
                "0.0.0.0" => "127.0.0.1".into(),
                "::" | "[::]" => "::1".into(),
                h => h.into(),
            },
        }
    }

    /// `argv` with the run's values in place.
    pub(crate) fn command(&self, argv: &[String], port: u16, id: &str, token: &str) -> Vec<String> {
        argv.iter()
            .map(|a| {
                a.replace("${hub_host}", &self.hub_host())
                    .replace("${hub_port}", &port.to_string())
                    .replace("${id}", id)
                    .replace("${token}", token)
                    .replace("${log_level}", "debug")
            })
            .collect()
    }
}

fn expand_env(text: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${env:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 6..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("{text:?}: `${{env:` without `}}`"))?;
        let name = &after[..end];
        let value = std::env::var(name)
            .map_err(|_| format!("{text:?}: the environment variable {name} isn't set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Calls a remote configurator endpoint the way the server does: the
/// request POSTed as JSON, the answer checked like a local configurator's,
/// returned files downloaded with the same client. Each call runs on a
/// thread of its own (callers may be on an async runtime).
pub(crate) fn configure_remote(
    inv: &Invocation,
    endpoint: &RemoteConfigurator,
) -> Result<Configured, Vec<String>> {
    let request = configure::remote_request(inv, endpoint.send_secrets)?;
    let body = serde_json::to_vec(&request).expect("a request serializes");
    let limits = inv.limits.clone();
    let url = &endpoint.url;
    let (status, body) = on_thread(endpoint, limits.timeout, |client, runtime| {
        runtime.block_on(async {
            let answer = authorize(
                client
                    .post(url)
                    .header("content-type", "application/json")
                    .body(body),
                endpoint,
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
            let status = answer.status();
            let body = read_limited(answer, limits.max_response_bytes).await?;
            Ok::<_, String>((status, body))
        })
    })
    .map_err(|e| vec![format!("{url}: {e}")])?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(vec![format!(
            "{url} answered {status}: {}",
            text.chars().take(500).collect::<String>()
        )]);
    }
    let response: RemoteResponse = serde_json::from_slice(&body)
        .map_err(|e| vec![format!("{url}: the answer isn't a configurator response: {e}")])?;
    let download = |url: &str, dest: &Path, max: u64| -> Result<(), String> {
        on_thread(endpoint, limits.timeout, |client, runtime| {
            runtime.block_on(async {
                let answer = authorize(client.get(url), endpoint)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !answer.status().is_success() {
                    return Err(format!("answered {}", answer.status()));
                }
                let bytes = read_limited(answer, max).await?;
                std::fs::write(dest, bytes).map_err(|e| e.to_string())
            })
        })
    };
    configure::remote_result(inv, response, &download)
}

/// Runs `f` with a client and a runtime of its own on a thread of its own
/// (a runtime can't be started or dropped on an async caller's thread), and
/// waits for it.
fn on_thread<T: Send>(
    endpoint: &RemoteConfigurator,
    timeout: Duration,
    f: impl FnOnce(&reqwest::Client, &tokio::runtime::Runtime) -> Result<T, String> + Send,
) -> Result<T, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                let client = client(endpoint, timeout)?;
                f(&client, &runtime)
            })
            .join()
            .unwrap_or_else(|_| Err("the call panicked".into()))
    })
}

fn authorize(request: reqwest::RequestBuilder, endpoint: &RemoteConfigurator) -> reqwest::RequestBuilder {
    match &endpoint.auth {
        Some(Auth::Basic { username, password }) => request.basic_auth(username, password.as_deref()),
        Some(Auth::Bearer { token }) => request.bearer_auth(token),
        _ => request,
    }
}

async fn read_limited(mut answer: reqwest::Response, max: u64) -> Result<Vec<u8>, String> {
    let mut body = vec![];
    while let Some(chunk) = answer.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > max {
            return Err(format!("more than {max} bytes"));
        }
    }
    Ok(body)
}

/// rustls (ring), the CA file or the bundled Mozilla roots, the client
/// certificate for mTLS; no redirects, as on the server.
fn client(endpoint: &RemoteConfigurator, timeout: Duration) -> Result<reqwest::Client, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    match &endpoint.ca {
        Some(path) => {
            let shown = path.display();
            for cert in CertificateDer::pem_file_iter(path).map_err(|e| format!("{shown}: {e}"))? {
                roots
                    .add(cert.map_err(|e| format!("{shown}: {e}"))?)
                    .map_err(|e| format!("{shown}: {e}"))?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots);
    let config = match &endpoint.auth {
        Some(Auth::Mtls { cert, key }) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .and_then(|c| c.collect::<Result<Vec<_>, _>>())
                .map_err(|e| format!("{}: {e}", cert.display()))?;
            let key_der =
                PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
            builder
                .with_client_auth_cert(chain, key_der)
                .map_err(|e| format!("client certificate: {e}"))?
        }
        _ => builder.with_no_client_auth(),
    };
    reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_section_and_fills_in_commands() {
        let config: crate::config::Config = toml::from_str(
            r#"
            [remote]
            start = ["start", "--host", "${hub_host}", "--port", "${hub_port}", "--id", "${id}"]
            host = "0.0.0.0"
            id = "edge"
            [remote.configurator]
            url = "https://edge:9443/configure"
            auth = { type = "mtls", cert = "c.pem", key = "k.pem" }
            "#,
        )
        .unwrap();
        let remote = config.remote.unwrap().resolve(Path::new("/p")).unwrap();
        assert_eq!(remote.hub_host(), "127.0.0.1");
        assert_eq!(
            remote.command(&remote.start, 7401, "edge", "t"),
            ["start", "--host", "127.0.0.1", "--port", "7401", "--id", "edge"]
        );
        assert_eq!(
            remote.configurator.unwrap().auth,
            Some(Auth::Mtls {
                cert: "/p/c.pem".into(),
                key: "/p/k.pem".into()
            })
        );
        assert!(expand_env("${env:TRELLIS_SURELY_UNSET_VAR}").is_err());
        assert_eq!(expand_env("plain").unwrap(), "plain");
    }
}
