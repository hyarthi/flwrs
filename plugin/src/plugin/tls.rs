//! TLS to the hub's TLS listener (feature `tls`), for remote plugins: the
//! hub's certificate is checked against a CA the plugin is given, and the
//! plugin can present a client certificate (mTLS).
//!
//! Settings come from [`TlsSettings`] in the `ConnectionConfig`, or from the
//! environment ([`TlsSettings::from_env`]), so an operator can enable TLS
//! for any SDK plugin without code changes.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// PEM file of the CA(s) that signed the hub's certificate. Setting it turns TLS on.
pub const TLS_CA_ENV: &str = "FLWRS_TLS_CA";
/// The name to check the hub's certificate against (default: the host).
pub const TLS_SERVER_NAME_ENV: &str = "FLWRS_TLS_SERVER_NAME";
/// PEM files of the plugin's client certificate (chain) and its private key (mTLS).
pub const TLS_CERT_ENV: &str = "FLWRS_TLS_CERT";
pub const TLS_KEY_ENV: &str = "FLWRS_TLS_KEY";

/// How to connect to the hub over TLS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsSettings {
    /// PEM file with the CA certificate(s) the hub's certificate must chain to.
    pub ca_file: PathBuf,
    /// Name the hub's certificate must be valid for; `None` = the host.
    pub server_name: Option<String>,
    /// Client certificate chain and key (PEM files), for mTLS.
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

impl TlsSettings {
    pub fn new(ca_file: impl Into<PathBuf>) -> Self {
        Self {
            ca_file: ca_file.into(),
            ..Default::default()
        }
    }

    pub fn with_server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = Some(name.into());
        self
    }

    /// The plugin's client certificate chain and private key (PEM files).
    pub fn with_client_cert(mut self, cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        self.client_cert = Some(cert.into());
        self.client_key = Some(key.into());
        self
    }

    /// From `FLWRS_TLS_CA` (required; unset = no TLS), `FLWRS_TLS_SERVER_NAME`,
    /// `FLWRS_TLS_CERT` and `FLWRS_TLS_KEY`.
    pub fn from_env() -> Option<Self> {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        Some(Self {
            ca_file: var(TLS_CA_ENV)?.into(),
            server_name: var(TLS_SERVER_NAME_ENV),
            client_cert: var(TLS_CERT_ENV).map(Into::into),
            client_key: var(TLS_KEY_ENV).map(Into::into),
        })
    }
}

/// A ready TLS client for one hub.
pub(crate) struct ClientTls {
    connector: TlsConnector,
    server_name: ServerName<'static>,
}

impl std::fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTls")
            .field("server_name", &self.server_name)
            .finish()
    }
}

fn invalid(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

impl ClientTls {
    pub(crate) fn new(settings: &TlsSettings, host: &str) -> io::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        let cas = CertificateDer::pem_file_iter(&settings.ca_file)
            .map_err(|e| invalid(format!("{}: {e}", settings.ca_file.display())))?;
        for ca in cas {
            let ca = ca.map_err(|e| invalid(format!("{}: {e}", settings.ca_file.display())))?;
            roots
                .add(ca)
                .map_err(|e| invalid(format!("{}: {e}", settings.ca_file.display())))?;
        }
        let builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid(e.to_string()))?
            .with_root_certificates(roots);
        let config = match (&settings.client_cert, &settings.client_key) {
            (Some(cert), Some(key)) => {
                let chain = CertificateDer::pem_file_iter(cert)
                    .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| invalid(format!("{}: {e}", cert.display())))?;
                let key = PrivateKeyDer::from_pem_file(key)
                    .map_err(|e| invalid(format!("{}: {e}", key.display())))?;
                builder
                    .with_client_auth_cert(chain, key)
                    .map_err(|e| invalid(format!("client certificate: {e}")))?
            }
            (None, None) => builder.with_no_client_auth(),
            _ => return Err(invalid("a client certificate needs both a cert and a key".into())),
        };
        let name = settings.server_name.as_deref().unwrap_or(host).to_string();
        let server_name =
            ServerName::try_from(name.clone()).map_err(|e| invalid(format!("{name:?}: {e}")))?;
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
        })
    }

    pub(crate) async fn connect(&self, stream: TcpStream) -> io::Result<TlsStream<TcpStream>> {
        self.connector
            .connect(self.server_name.clone(), stream)
            .await
    }
}
