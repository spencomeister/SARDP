//! Test-only TLS certificate generation for loopback QUIC connections
//! (PoC brief: "TLS証明書: ループバック検証用にrcgenでテスト用CAと証明書を
//! 生成する。証明書検証を丸ごと無効化する実装は避ける").
//!
//! This generates a single self-signed certificate and trusts it directly
//! as a root on the client side. Certificate *validation* itself is not
//! bypassed: the client's `rustls::RootCertStore` only contains this one
//! certificate, so the normal X.509 path-building/signature/validity/
//! hostname checks in `rustls-webpki` still run against it during the
//! handshake, they just succeed because the presented leaf equals the
//! trusted root (a standard self-signed-cert-as-root test setup, the same
//! pattern quinn's own test suite uses).

use std::sync::Arc;

use quinn::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use quinn::rustls::RootCertStore;

/// A self-signed test certificate/key pair plus the DER bytes a client
/// needs to trust it.
pub struct TestCertificate {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivatePkcs8KeyDer<'static>,
}

/// Generates a fresh self-signed certificate valid for `hostname` (e.g.
/// `"localhost"`). **Test-only**: not a real CA, not for production use.
pub fn generate_test_certificate(hostname: &str) -> TestCertificate {
    let certified_key = rcgen::generate_simple_self_signed(vec![hostname.to_string()])
        .expect("self-signed cert generation cannot fail for a valid hostname");
    let key_der = PrivatePkcs8KeyDer::from(certified_key.signing_key.serialize_der());
    let cert_der = CertificateDer::from(certified_key.cert);
    TestCertificate { cert_der, key_der }
}

/// Builds a `RootCertStore` that trusts exactly one certificate.
pub fn root_store_trusting(cert_der: &CertificateDer<'static>) -> Arc<RootCertStore> {
    let mut roots = RootCertStore::empty();
    roots
        .add(cert_der.clone())
        .expect("a freshly generated CertificateDer is well-formed DER");
    Arc::new(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_nonempty_certificate_and_key() {
        let test_cert = generate_test_certificate("localhost");
        assert!(!test_cert.cert_der.is_empty());
        assert!(!test_cert.key_der.secret_pkcs8_der().is_empty());
    }

    #[test]
    fn root_store_trusts_the_generated_certificate() {
        let test_cert = generate_test_certificate("localhost");
        let roots = root_store_trusting(&test_cert.cert_der);
        assert_eq!(roots.len(), 1);
    }
}
