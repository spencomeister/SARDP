//! QUIC endpoint setup (spec 5.1: ALPN `sardp/1`), built on `quinn`.

use std::net::SocketAddr;
use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::rustls::RootCertStore;
use quinn::rustls::pki_types::CertificateDer;
use quinn::{ClientConfig, Endpoint, ServerConfig};

use crate::pki::TestCertificate;

/// ALPN protocol id for the QUIC binding (spec 2.1, DR-016).
pub const ALPN: &[u8] = b"sardp/1";

fn crypto_provider() -> Arc<quinn::rustls::crypto::CryptoProvider> {
    Arc::new(quinn::rustls::crypto::ring::default_provider())
}

/// Builds a QUIC server endpoint bound to `bind_addr`, presenting
/// `test_cert` and negotiating ALPN `sardp/1`.
pub fn server_endpoint(bind_addr: SocketAddr, test_cert: &TestCertificate) -> Endpoint {
    let mut rustls_config = quinn::rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&quinn::rustls::version::TLS13])
        .expect("TLS 1.3 is supported by the ring provider")
        .with_no_client_auth()
        .with_single_cert(
            vec![test_cert.cert_der.clone()],
            test_cert.key_der.clone_key().into(),
        )
        .expect("freshly generated test cert/key are well-formed");
    rustls_config.alpn_protocols = vec![ALPN.to_vec()];
    // Spec 2.3 / Part 3: "初期版は0-RTT自体を無効化する" (0-RTT is disabled
    // in this version) -- MUST NOT accept early data. Leave
    // `max_early_data_size` at rustls's default of 0 rather than
    // `u32::MAX` (QUIC requires it be exactly one of the two).

    let quic_server_config = QuicServerConfig::try_from(rustls_config)
        .expect("TLS 1.3 is enabled, satisfying QuicServerConfig's requirement");
    let server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));

    Endpoint::server(server_config, bind_addr).expect("binding a loopback UDP socket")
}

/// Builds a QUIC client endpoint bound to `bind_addr`, trusting only
/// `trusted_root` and negotiating ALPN `sardp/1`.
pub fn client_endpoint(bind_addr: SocketAddr, trusted_root: &CertificateDer<'static>) -> Endpoint {
    let mut roots = RootCertStore::empty();
    roots
        .add(trusted_root.clone())
        .expect("a freshly generated CertificateDer is well-formed DER");

    let mut rustls_config = quinn::rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&quinn::rustls::version::TLS13])
        .expect("TLS 1.3 is supported by the ring provider")
        .with_root_certificates(roots)
        .with_no_client_auth();
    rustls_config.alpn_protocols = vec![ALPN.to_vec()];

    let quic_client_config = QuicClientConfig::try_from(rustls_config)
        .expect("TLS 1.3 is enabled, satisfying QuicClientConfig's requirement");
    let client_config = ClientConfig::new(Arc::new(quic_client_config));

    let mut endpoint = Endpoint::client(bind_addr).expect("binding a loopback UDP socket");
    endpoint.set_default_client_config(client_config);
    endpoint
}
