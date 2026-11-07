//! The TLS listener and remote authentication (feature `remote`): tokens
//! that require TLS, client certificates checked against the client CA and
//! the node's name, and pinned fingerprints. Certificates are made per test
//! with rcgen.

use super::config::{HubConfig, HubTlsConfig};
use super::service::{ExpectedNode, HubEvent, NodeAuth};
use super::tests::{
    FakePlugin, TOKEN, assert_quiet, child_succeeded, config, expected, init, next,
    spawn_child_with, start_hub,
};
use crate::modules::plugin::manifest::PluginType;
use flwrs_plugin::schema::protocol::{InitializeResponse, plugin_type::Enum as PbPluginType};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpStream;

/// A test CA, and certificates it (or nobody) signs, as PEM files.
struct Pki {
    dir: tempfile::TempDir,
    ca: CertifiedIssuer<'static, KeyPair>,
}

/// A certificate and its key, on disk and in memory.
struct Leaf {
    cert: PathBuf,
    key: PathBuf,
    der: CertificateDer<'static>,
    key_der: Vec<u8>,
}

impl Leaf {
    fn sha256(&self) -> String {
        sha256::digest(self.der.as_ref())
    }
}

impl Pki {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, "flwrs test CA");
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ca.pem"), ca.pem()).unwrap();
        Self { dir, ca }
    }

    fn ca_file(&self) -> PathBuf {
        self.dir.path().join("ca.pem")
    }

    /// A certificate for `names` (SAN DNS) with CN `cn`, signed by the CA,
    /// or self-signed.
    fn leaf(&self, file: &str, names: &[&str], cn: &str, self_signed: bool) -> Leaf {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let mut params = CertificateParams::new(names).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        let key = KeyPair::generate().unwrap();
        let cert = if self_signed {
            params.self_signed(&key).unwrap()
        } else {
            params.signed_by(&key, &self.ca).unwrap()
        };
        let path = |ext: &str| self.dir.path().join(format!("{file}.{ext}"));
        std::fs::write(path("pem"), cert.pem()).unwrap();
        std::fs::write(path("key"), key.serialize_pem()).unwrap();
        Leaf {
            cert: path("pem"),
            key: path("key"),
            der: cert.der().clone(),
            key_der: key.serialize_der(),
        }
    }

    fn hub_config(&self) -> HubConfig {
        let server = self.leaf("server", &["localhost"], "flwrs hub", false);
        HubConfig {
            tls: Some(HubTlsConfig {
                host: "127.0.0.1".into(),
                port: 0,
                cert: server.cert,
                key: server.key,
                client_ca: Some(self.ca_file()),
            }),
            ..config()
        }
    }

    /// A TLS connection to the hub's TLS listener, trusting the CA, with an
    /// optional client certificate.
    async fn connect(&self, addr: std::net::SocketAddr, client: Option<&Leaf>) -> FakePlugin {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca.der().clone()).unwrap();
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
        let config = match client {
            Some(leaf) => builder
                .with_client_auth_cert(
                    vec![leaf.der.clone()],
                    PrivateKeyDer::try_from(leaf.key_der.clone()).unwrap(),
                )
                .unwrap(),
            None => builder.with_no_client_auth(),
        };
        let stream = TcpStream::connect(addr).await.unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
            .unwrap();
        FakePlugin::over(Box::new(tls))
    }
}

fn remote(node_id: &str, instance_id: &str, auth: NodeAuth) -> ExpectedNode {
    ExpectedNode {
        instance_id: instance_id.into(),
        auth,
        ..expected(node_id, PluginType::Sink)
    }
}

/// Sends a sink's Initialize; true if it's accepted.
async fn accepted(plugin: &mut FakePlugin, instance_id: &str, token: &str) -> bool {
    plugin
        .send(&init(PbPluginType::Sink, instance_id, token))
        .await;
    match plugin.recv().await {
        Some(frame) => frame.decode::<InitializeResponse>().is_ok(),
        None => false,
    }
}

#[tokio::test]
async fn token_nodes_can_require_tls() {
    let pki = Pki::new();
    let hub = start_hub(pki.hub_config()).await;
    let tls_addr = hub.hub.tls_addr().expect("a TLS listener");
    let mut link = hub.hub.expect(remote(
        "n",
        "gateway",
        NodeAuth::Token {
            token: TOKEN.into(),
            require_tls: true,
        },
    ));

    // The right token over plain TCP isn't enough.
    let mut plain = FakePlugin::connect(&hub.hub).await;
    assert!(!accepted(&mut plain, "gateway", TOKEN).await);
    assert_quiet(&mut link).await;

    // Over TLS (no client certificate needed) it is; a wrong token isn't.
    let mut wrong = pki.connect(tls_addr, None).await;
    assert!(!accepted(&mut wrong, "gateway", "wrong-token-wrong-token").await);
    let mut tls = pki.connect(tls_addr, None).await;
    assert!(accepted(&mut tls, "gateway", TOKEN).await);
    assert!(matches!(next(&mut link).await, HubEvent::Connected { .. }));
}

#[tokio::test]
async fn mtls_nodes_need_a_certificate_from_the_ca_naming_them() {
    let pki = Pki::new();
    let hub = start_hub(pki.hub_config()).await;
    let tls_addr = hub.hub.tls_addr().unwrap();
    let mut link = hub
        .hub
        .expect(remote("n", "gateway-1", NodeAuth::Mtls { pin: None }));

    let by_san = pki.leaf("by-san", &["gateway-1"], "something else", false);
    let by_cn = pki.leaf("by-cn", &[], "gateway-1", false);
    let other_name = pki.leaf("other", &["gateway-2"], "gateway-2", false);
    let self_signed = pki.leaf("self", &["gateway-1"], "gateway-1", true);

    let mut open = vec![]; // keeps accepted connections up
    for (leaf, ok) in [
        (Some(&other_name), false),
        (Some(&self_signed), false),
        (None, false),
        (Some(&by_cn), true),
        (Some(&by_san), true),
    ] {
        let mut plugin = pki.connect(tls_addr, leaf).await;
        // No token: the certificate is the credential.
        assert_eq!(accepted(&mut plugin, "gateway-1", "").await, ok);
        if ok {
            assert!(matches!(next(&mut link).await, HubEvent::Connected { .. }));
        }
        open.push(plugin);
    }
    // Plain TCP never works for an mTLS node.
    let mut plain = FakePlugin::connect(&hub.hub).await;
    assert!(!accepted(&mut plain, "gateway-1", "").await);
}

#[tokio::test]
async fn mtls_nodes_can_pin_a_self_signed_certificate() {
    let pki = Pki::new();
    let hub = start_hub(pki.hub_config()).await;
    let tls_addr = hub.hub.tls_addr().unwrap();
    let pinned = pki.leaf("pinned", &[], "anything", true);
    let other = pki.leaf("other", &["gateway"], "gateway", false);
    let mut link = hub.hub.expect(remote(
        "n",
        "gateway",
        NodeAuth::Mtls {
            pin: Some(pinned.sha256()),
        },
    ));
    // Even a CA-signed certificate naming the node doesn't do: the pin decides.
    let mut plugin = pki.connect(tls_addr, Some(&other)).await;
    assert!(!accepted(&mut plugin, "gateway", "").await);
    let mut plugin = pki.connect(tls_addr, Some(&pinned)).await;
    assert!(accepted(&mut plugin, "gateway", "").await);
    assert!(matches!(next(&mut link).await, HubEvent::Connected { .. }));
}

#[tokio::test]
async fn the_sdk_connects_with_a_client_certificate_from_the_environment() {
    let pki = Pki::new();
    let hub = start_hub(pki.hub_config()).await;
    let tls_addr = hub.hub.tls_addr().unwrap();
    let mut node = expected("upper", PluginType::Transform);
    node.auth = NodeAuth::Mtls { pin: None };
    let mut link = hub.hub.expect(node);
    let client = pki.leaf("client", &["upper"], "upper", false);
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let child = spawn_child_with(
        tls_addr.port(),
        "",
        "ok",
        &[
            ("FLWRS_TLS_CA", path(&pki.ca_file())),
            ("FLWRS_TLS_SERVER_NAME", "localhost".into()),
            ("FLWRS_TLS_CERT", path(&client.cert)),
            ("FLWRS_TLS_KEY", path(&client.key)),
        ],
    );
    assert!(matches!(next(&mut link).await, HubEvent::Connected { .. }));
    link.shutdown("").await.unwrap();
    assert!(matches!(next(&mut link).await, HubEvent::Exit { .. }));
    child_succeeded(child).await;
}
