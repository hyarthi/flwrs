//! The TLS client configuration (rustls with ring): the broker's
//! certificate checked against an uploaded CA or the bundled Mozilla roots,
//! an optional client certificate (mTLS), and a development-only switch that
//! skips verification.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::path::Path;
use std::sync::Arc;

pub struct TlsFiles<'a> {
    pub ca: Option<&'a Path>,
    pub cert: Option<&'a Path>,
    pub key: Option<&'a Path>,
    pub insecure_skip_verify: bool,
}

pub fn client_config(files: &TlsFiles) -> Result<Arc<ClientConfig>, Vec<String>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut problems = vec![];
    let mut roots = RootCertStore::empty();
    match files.ca {
        Some(ca) => match CertificateDer::pem_file_iter(ca)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        {
            Ok(certs) if certs.is_empty() => {
                problems.push(format!("--tls-ca {}: no certificate in it", ca.display()))
            }
            Ok(certs) => {
                for cert in certs {
                    if let Err(e) = roots.add(cert) {
                        problems.push(format!("--tls-ca {}: {e}", ca.display()));
                    }
                }
            }
            Err(e) => problems.push(format!("--tls-ca {}: {e}", ca.display())),
        },
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let client_auth = match (files.cert, files.key) {
        (Some(cert), Some(key)) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                .map_err(|e| problems.push(format!("--tls-cert {}: {e}", cert.display())))
                .ok();
            let key = PrivateKeyDer::from_pem_file(key)
                .map_err(|e| problems.push(format!("--tls-key {}: {e}", key.display())))
                .ok();
            chain.zip(key)
        }
        (None, None) => None,
        _ => {
            problems
                .push("tls_cert and tls_key go together (a client certificate and its key)".into());
            None
        }
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| vec![e.to_string()])?
        .with_root_certificates(roots);
    let mut config = match client_auth {
        Some((chain, key)) => builder
            .with_client_auth_cert(chain, key)
            .map_err(|e| vec![format!("client certificate: {e}")])?,
        None => builder.with_no_client_auth(),
    };
    if files.insecure_skip_verify {
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoVerification(provider)));
    }
    Ok(Arc::new(config))
}

/// Accepts any certificate (signatures are still checked): for development
/// against brokers with self-signed certificates only.
#[derive(Debug)]
struct NoVerification(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
