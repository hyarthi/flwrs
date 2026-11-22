//! Plugins' configurators (manifest `configurator`, see `docs/plugin-spec.md`
//! and `flwrs_plugin::package::configure`): programs in a package that set
//! some of a local node's launch args from its other settings. The scene
//! service runs them when a node's settings or a named upload change (and on
//! `POST …/configure`), the plugin registry on upgrade. Remote nodes never
//! run one: their configured fields are set by the user.

use crate::modules::plugin::service::Plugin;
use crate::modules::scene::service::{Store, stored_name};
use flwrs_plugin::package::configure::{self, Configured, Invocation};
use flwrs_plugin::package::{LaunchSpec, Manifest};
use serde_json::Value;
use sqlx::{AssertSqlSafe, SqliteConnection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) use configure::Limits;

/// Where uploads live, and the limits of a run.
#[derive(Clone, Debug)]
pub(crate) struct Env {
    /// `[director] files_dir`.
    pub files_dir: PathBuf,
    /// `[plugins] configure_timeout_ms` / `configure_max_response_bytes`;
    /// files at most `[scenes] max_upload_bytes`.
    pub limits: Limits,
}

/// A package's parts a configurator run needs.
#[derive(Clone, Debug)]
pub(crate) struct PackageRef {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub launch_args_schema: Option<String>,
}

impl PackageRef {
    pub(crate) fn of(plugin: &Plugin) -> Option<Self> {
        Some(Self {
            dir: PathBuf::from(&plugin.install_path),
            manifest: plugin.parsed_manifest()?,
            launch_args_schema: plugin.launch_args_schema.clone(),
        })
    }

    pub(crate) fn has_configurator(&self) -> bool {
        self.manifest.configurator.is_some()
    }
}

/// A node's uploads: name → stored file.
pub(crate) async fn uploads(
    conn: &mut SqliteConnection,
    store: Store,
    files_dir: &Path,
    node_id: &str,
) -> Result<HashMap<String, PathBuf>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT \"name\", sha256 FROM {} WHERE node_id = $1",
        store.uploads()
    )))
    .bind(node_id)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, sha)| {
            let path = files_dir.join(stored_name(&name, &sha));
            (name, path)
        })
        .collect())
}

/// Builds the invocation (the plugin's rules) and calls `f` with it.
fn with_invocation<T>(
    package: &PackageRef,
    launch_args: &Value,
    node_id: &str,
    uploads: &HashMap<String, PathBuf>,
    limits: &Limits,
    f: impl FnOnce(&Invocation) -> Result<T, Vec<String>>,
) -> Result<T, Vec<String>> {
    let schema: Option<Value> = match &package.launch_args_schema {
        Some(text) => {
            Some(serde_json::from_str(text).map_err(|e| vec![format!("launch_args_schema: {e}")])?)
        }
        None => None,
    };
    let validator = match &schema {
        Some(s) => Some(
            jsonschema::validator_for(s).map_err(|e| vec![format!("launch_args_schema: {e}")])?,
        ),
        None => None,
    };
    let (spec, _) = LaunchSpec::new(&package.manifest, schema.as_ref());
    let lookup = |name: &str| uploads.get(name).cloned();
    f(&Invocation {
        package_dir: &package.dir,
        executable: None,
        manifest: &package.manifest,
        spec: &spec,
        schema: validator.as_ref(),
        launch_args,
        uploads: &lookup,
        node_id,
        limits,
    })
}

/// Runs the package's configurator for a node (on a blocking thread).
pub(crate) async fn run(
    package: &PackageRef,
    launch_args: &Value,
    node_id: &str,
    uploads: HashMap<String, PathBuf>,
    limits: &Limits,
) -> Result<Configured, Vec<String>> {
    let (package, launch_args, node_id, limits) = (
        package.clone(),
        launch_args.clone(),
        node_id.to_string(),
        limits.clone(),
    );
    tokio::task::spawn_blocking(move || {
        with_invocation(
            &package,
            &launch_args,
            &node_id,
            &uploads,
            &limits,
            configure::configure,
        )
    })
    .await
    .map_err(|e| vec![format!("the configurator's thread failed: {e}")])?
}

/// Whether `url` is within one of the `allowed` prefixes: the same, or
/// below it at a `/` or `?` boundary (so `https://a.com` doesn't allow
/// `https://a.com.evil.net`).
pub(crate) fn url_allowed(allowed: &[String], url: &str) -> bool {
    allowed
        .iter()
        .any(|prefix| match url.strip_prefix(prefix.as_str()) {
            Some("") => true,
            Some(rest) => prefix.ends_with('/') || rest.starts_with(['/', '?']),
            None => false,
        })
}

/// A remote node's configurator endpoint (`docs/plugin-spec.md` §4.1): the
/// request POSTed as JSON, the answer checked like a local one's; returned
/// files downloaded with the same client. Every URL must be within
/// `allowed` (`[plugins] remote_configurator_urls`).
#[cfg(feature = "remote")]
pub(crate) async fn run_remote(
    package: &PackageRef,
    remote: &crate::modules::node_config::RemoteConfigurator,
    launch_args: &Value,
    node_id: &str,
    uploads: HashMap<String, PathBuf>,
    limits: &Limits,
    allowed: &[String],
) -> Result<Configured, Vec<String>> {
    use crate::modules::node_config::ConfiguratorAuth;
    if !url_allowed(allowed, &remote.url) {
        return Err(vec![format!(
            "{} isn't within [plugins] remote_configurator_urls (empty: remote configurators are off)",
            remote.url
        )]);
    }
    let client = remote_client(remote, &uploads, limits).map_err(|e| vec![e])?;
    let authorize = {
        let auth = remote.auth.clone();
        move |request: reqwest::RequestBuilder| match &auth {
            Some(ConfiguratorAuth::Basic { username, password }) => {
                request.basic_auth(username, password.as_deref())
            }
            Some(ConfiguratorAuth::Bearer { token }) => {
                request.bearer_auth(token.as_deref().unwrap_or_default())
            }
            _ => request,
        }
    };
    let (pkg, args, id, lim, ups, send) = (
        package.clone(),
        launch_args.clone(),
        node_id.to_string(),
        limits.clone(),
        uploads.clone(),
        remote.send_secrets,
    );
    let request = tokio::task::spawn_blocking(move || {
        with_invocation(&pkg, &args, &id, &ups, &lim, |inv| {
            configure::remote_request(inv, send)
        })
    })
    .await
    .map_err(|e| vec![e.to_string()])??;
    let body = serde_json::to_vec(&request).expect("a request serializes");
    let answer = authorize(
        client
            .post(&remote.url)
            .header("content-type", "application/json")
            .body(body),
    )
    .send()
    .await
    .map_err(|e| vec![format!("{}: {e}", remote.url)])?;
    let status = answer.status();
    let body = read_limited(answer, limits.max_response_bytes)
        .await
        .map_err(|e| vec![format!("{}: {e}", remote.url)])?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(vec![format!(
            "{} answered {status}: {}",
            remote.url,
            text.chars().take(500).collect::<String>()
        )]);
    }
    let response: configure::RemoteResponse = serde_json::from_slice(&body).map_err(|e| {
        vec![format!(
            "{}: the answer isn't a configurator response: {e}",
            remote.url
        )]
    })?;
    let (pkg, args, id, lim) = (
        package.clone(),
        launch_args.clone(),
        node_id.to_string(),
        limits.clone(),
    );
    let allowed = allowed.to_vec();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let download = |url: &str, dest: &Path, max: u64| -> Result<(), String> {
            if !url_allowed(&allowed, url) {
                return Err("not within [plugins] remote_configurator_urls".into());
            }
            runtime.block_on(async {
                let answer = authorize(client.get(url))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !answer.status().is_success() {
                    return Err(format!("answered {}", answer.status()));
                }
                let bytes = read_limited(answer, max).await?;
                std::fs::write(dest, bytes).map_err(|e| e.to_string())
            })
        };
        with_invocation(&pkg, &args, &id, &uploads, &lim, |inv| {
            configure::remote_result(inv, response, &download)
        })
    })
    .await
    .map_err(|e| vec![e.to_string()])?
}

/// A response's body, refused beyond `max` bytes.
#[cfg(feature = "remote")]
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

/// The HTTP client for a remote configurator: rustls (ring), the node's CA
/// upload or the bundled Mozilla roots, its client certificate for mTLS; no
/// redirects (a redirect could leave the allowed URLs).
#[cfg(feature = "remote")]
fn remote_client(
    remote: &crate::modules::node_config::RemoteConfigurator,
    uploads: &HashMap<String, PathBuf>,
    limits: &Limits,
) -> Result<reqwest::Client, String> {
    use crate::modules::node_config::ConfiguratorAuth;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let upload = |name: &str| {
        uploads
            .get(name)
            .cloned()
            .ok_or_else(|| format!("the node's upload {name:?} is missing"))
    };
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    match &remote.ca {
        Some(name) => {
            for cert in
                CertificateDer::pem_file_iter(upload(name)?).map_err(|e| format!("{name}: {e}"))?
            {
                roots
                    .add(cert.map_err(|e| format!("{name}: {e}"))?)
                    .map_err(|e| format!("{name}: {e}"))?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots);
    let config = match &remote.auth {
        Some(ConfiguratorAuth::Mtls { cert, key }) => {
            let chain = CertificateDer::pem_file_iter(upload(cert)?)
                .and_then(|c| c.collect::<Result<Vec<_>, _>>())
                .map_err(|e| format!("{cert}: {e}"))?;
            let key_der =
                PrivateKeyDer::from_pem_file(upload(key)?).map_err(|e| format!("{key}: {e}"))?;
            builder
                .with_client_auth_cert(chain, key_der)
                .map_err(|e| format!("client certificate: {e}"))?
        }
        _ => builder.with_no_client_auth(),
    };
    reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(limits.timeout)
        .build()
        .map_err(|e| e.to_string())
}

/// Stores the files a configurator returned as the node's uploads (the
/// caller's transaction; the stored files themselves are written first and
/// are collected as garbage if the transaction doesn't commit).
pub(crate) async fn store_files(
    conn: &mut SqliteConnection,
    store: Store,
    files_dir: &Path,
    node_id: &str,
    configured: &Configured,
) -> Result<(), String> {
    for file in &configured.files {
        let (source, dir) = (file.path.clone(), files_dir.to_path_buf());
        let name = file.name.clone();
        let (sha, size) = tokio::task::spawn_blocking(move || -> std::io::Result<(String, u64)> {
            let size = std::fs::metadata(&source)?.len();
            let sha = sha256::try_digest(source.as_path())?;
            let stored = dir.join(stored_name(&name, &sha));
            if !stored.exists() {
                std::fs::create_dir_all(&dir)?;
                let partial = dir.join(format!(".configured-{}", ulid::Ulid::generate()));
                std::fs::copy(&source, &partial)?;
                std::fs::rename(&partial, &stored)?;
                let mut perms = std::fs::metadata(&stored)?.permissions();
                perms.set_readonly(true);
                std::fs::set_permissions(&stored, perms)?;
            }
            Ok((sha, size))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("storing {}: {e}", file.name))?;
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (node_id, \"name\", sha256, \"size\", create_time) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (node_id, \"name\") DO UPDATE SET sha256 = excluded.sha256, \
                 \"size\" = excluded.\"size\", create_time = excluded.create_time",
            store.uploads()
        )))
        .bind(node_id)
        .bind(&file.name)
        .bind(&sha)
        .bind(i64::try_from(size).unwrap_or(i64::MAX))
        .bind(chrono::Local::now())
        .execute(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    }
    store_warnings(conn, node_id, configured)
        .await
        .map_err(|e| e.to_string())
}

/// How many runs' warnings are kept per node (live, the draft, and a few
/// states its undo history may return to).
const WARNING_RUNS_KEPT: i64 = 8;

/// The key a run's warnings are stored under: the node's launch args after
/// the run (as the node stores them), canonical JSON hashed.
pub(crate) fn warnings_key(launch_args: &Value) -> String {
    sha256::digest(crate::modules::scene::service::draft::canonical(launch_args).to_string())
}

/// Records a run's warnings (none: nothing kept for these settings), for
/// `check`. Live and the draft share the rows: each finds its settings' run.
async fn store_warnings(
    conn: &mut SqliteConnection,
    node_id: &str,
    configured: &Configured,
) -> Result<(), sqlx::Error> {
    let key = warnings_key(&configured.launch_args);
    if configured.warnings.is_empty() {
        sqlx::query(
            "DELETE FROM scene_node_configure_warnings WHERE node_id = $1 AND args_key = $2",
        )
        .bind(node_id)
        .bind(&key)
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO scene_node_configure_warnings (node_id, args_key, warnings, run_time) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (node_id, args_key) DO UPDATE SET warnings = excluded.warnings, \
             run_time = excluded.run_time",
    )
    .bind(node_id)
    .bind(&key)
    .bind(serde_json::to_string(&configured.warnings).expect("strings serialize"))
    .bind(chrono::Local::now())
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM scene_node_configure_warnings WHERE node_id = $1 AND args_key NOT IN \
         (SELECT args_key FROM scene_node_configure_warnings WHERE node_id = $1 \
          ORDER BY run_time DESC LIMIT $2)",
    )
    .bind(node_id)
    .bind(WARNING_RUNS_KEPT)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The warnings of the run that produced these launch args, with its time.
pub(crate) async fn warnings(
    conn: &mut SqliteConnection,
    node_id: &str,
    launch_args: &Value,
) -> Result<Option<(Vec<String>, chrono::DateTime<chrono::Local>)>, sqlx::Error> {
    let row: Option<(String, chrono::DateTime<chrono::Local>)> = sqlx::query_as(
        "SELECT warnings, run_time FROM scene_node_configure_warnings \
         WHERE node_id = $1 AND args_key = $2",
    )
    .bind(node_id)
    .bind(warnings_key(launch_args))
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(text, time)| (serde_json::from_str(&text).unwrap_or_default(), time)))
}

/// Drops the warnings of nodes that are in neither the live graph nor a draft.
pub(crate) async fn forget_warnings(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM scene_node_configure_warnings WHERE node_id NOT IN \
         (SELECT id FROM scene_nodes UNION SELECT id FROM scene_draft_nodes)",
    )
    .execute(conn)
    .await?;
    Ok(())
}
