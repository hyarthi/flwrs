//! The server's TLS (rustls with ring): its certificate and key, ALPN for
//! HTTP/2 and HTTP/1.1, and optionally client certificates checked against a
//! client CA (mTLS).

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use std::path::Path;
use std::sync::Arc;

/// A verified client certificate: its SHA-256 fingerprint (lowercase hex)
/// and names (SAN DNS and URI entries, then the CN).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCert {
    pub sha256: String,
    pub names: Vec<String>,
}

impl ClientCert {
    pub fn of(cert: &CertificateDer<'_>) -> Self {
        Self {
            sha256: sha256::digest(cert.as_ref()),
            names: cert_names(cert),
        }
    }
}

fn cert_names(cert: &CertificateDer<'_>) -> Vec<String> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let Ok((_, cert)) = X509Certificate::from_der(cert.as_ref()) else {
        return vec![];
    };
    let mut names = vec![];
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(n) | GeneralName::URI(n) = name {
                names.push((*n).to_string());
            }
        }
    }
    for cn in cert.subject().iter_common_name() {
        if let Ok(cn) = cn.as_str() {
            names.push(cn.to_string());
        }
    }
    names
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuth {
    /// Clients may present a certificate (checked if they do).
    Optional,
    Required,
}

pub struct TlsFiles<'a> {
    pub cert: &'a Path,
    pub key: &'a Path,
    pub client_ca: Option<&'a Path>,
    pub client_auth: ClientAuth,
}

fn certs(path: &Path, what: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|c| c.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("{what} {}: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!("{what} {}: no certificate in it", path.display()));
    }
    Ok(certs)
}

pub fn server_config(files: &TlsFiles) -> Result<Arc<ServerConfig>, Vec<String>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut problems = vec![];
    let chain = certs(files.cert, "tls_cert")
        .map_err(|e| problems.push(e))
        .ok();
    let key = PrivateKeyDer::from_pem_file(files.key)
        .map_err(|e| problems.push(format!("tls_key {}: {e}", files.key.display())))
        .ok();
    let verifier = files.client_ca.and_then(|ca| {
        let mut roots = rustls::RootCertStore::empty();
        for cert in certs(ca, "tls_client_ca")
            .map_err(|e| problems.push(e))
            .ok()?
        {
            if let Err(e) = roots.add(cert) {
                problems.push(format!("tls_client_ca {}: {e}", ca.display()));
            }
        }
        let builder =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone());
        let builder = match files.client_auth {
            ClientAuth::Optional => builder.allow_unauthenticated(),
            ClientAuth::Required => builder,
        };
        builder
            .build()
            .map_err(|e| problems.push(format!("tls_client_ca: {e}")))
            .ok()
    });
    let (Some(chain), Some(key)) = (chain, key) else {
        return Err(problems);
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    let builder = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| vec![e.to_string()])?;
    let builder = match verifier {
        Some(v) => builder.with_client_cert_verifier(v),
        None => builder.with_no_client_auth(),
    };
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| vec![format!("tls_cert / tls_key: {e}")])?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
