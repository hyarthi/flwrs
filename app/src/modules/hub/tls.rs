//! The hub's TLS listener for remote plugins (feature `remote`).
//!
//! Client certificates are optional at the TLS level (token nodes don't send
//! one), and any certificate is accepted during the handshake, so pinned
//! self-signed certificates work too. The handshake still proves the client
//! holds the certificate's key. Afterwards the hub records whether the
//! certificate chains to `[hub.tls] client_ca`, its SHA-256 fingerprint and
//! its names (SAN DNS and URI entries, and the subject CN); the node's
//! `auth` decides which of those must match (see `service::authenticate`).

use super::config::HubTlsConfig;
use super::service::ClientCert;
use rustls::DistinguishedName;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::io;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The bound TLS listener.
pub(crate) struct TlsListener {
    pub listener: TcpListener,
    pub acceptor: TlsAcceptor,
    /// Checks client certificates against `client_ca` (after the handshake).
    ca: Option<Arc<dyn ClientCertVerifier>>,
}

fn invalid(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

pub(crate) async fn bind(config: &HubTlsConfig) -> io::Result<TlsListener> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let chain = CertificateDer::pem_file_iter(&config.cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| invalid(format!("[hub.tls] cert {}: {e}", config.cert.display())))?;
    let key = PrivateKeyDer::from_pem_file(&config.key)
        .map_err(|e| invalid(format!("[hub.tls] key {}: {e}", config.key.display())))?;
    let ca: Option<Arc<dyn ClientCertVerifier>> = match &config.client_ca {
        None => None,
        Some(path) => {
            let mut roots = rustls::RootCertStore::empty();
            let certs = CertificateDer::pem_file_iter(path)
                .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                .map_err(|e| invalid(format!("[hub.tls] client_ca {}: {e}", path.display())))?;
            for cert in certs {
                roots
                    .add(cert)
                    .map_err(|e| invalid(format!("[hub.tls] client_ca: {e}")))?;
            }
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                    .build()
                    .map_err(|e| invalid(format!("[hub.tls] client_ca: {e}")))?;
            Some(verifier)
        }
    };
    let verifier = Arc::new(AnyClientCert {
        hints: ca
            .as_ref()
            .map(|ca| ca.root_hint_subjects().to_vec())
            .unwrap_or_default(),
        algorithms: provider.signature_verification_algorithms,
    });
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| invalid(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|e| invalid(format!("[hub.tls]: {e}")))?;
    let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
    Ok(TlsListener {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server)),
        ca,
    })
}

impl TlsListener {
    /// What the connection's client certificate says, if it sent one.
    pub(crate) fn client_cert(&self, certs: Option<&[CertificateDer<'_>]>) -> Option<ClientCert> {
        let (end_entity, intermediates) = certs?.split_first()?;
        let ca_verified = self.ca.as_ref().is_some_and(|ca| {
            ca.verify_client_cert(end_entity, intermediates, UnixTime::now())
                .is_ok()
        });
        Some(ClientCert {
            sha256: sha256::digest(end_entity.as_ref()),
            names: cert_names(end_entity),
            ca_verified,
        })
    }
}

/// The certificate's SAN DNS names and URIs, and its subject CN.
fn cert_names(cert: &CertificateDer<'_>) -> Vec<String> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let Ok((_, cert)) = X509Certificate::from_der(cert.as_ref()) else {
        return vec![];
    };
    let mut names = vec![];
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(n) | GeneralName::URI(n) => names.push((*n).to_string()),
                _ => {}
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

/// Asks for a client certificate, doesn't require one, and accepts any:
/// what it must match is decided per node after the handshake. The
/// handshake signature is still verified (the client holds the key).
#[derive(Debug)]
struct AnyClientCert {
    hints: Vec<DistinguishedName>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hints
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

