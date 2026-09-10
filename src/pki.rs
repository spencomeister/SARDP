//! TLS certificate handling for QUIC endpoints: test-only self-signed
//! generation (PoC brief: "TLS証明書: ループバック検証用にrcgenでテスト用CAと
//! 証明書を生成する。証明書検証を丸ごと無効化する実装は避ける"), plus loading
//! a real certificate/key pair from files for the `sardp-server`/
//! `sardp-client` binaries (Phase 1: "証明書/鍵のパスを受け取り、なければ
//! pkiモジュールで自己署名生成").
//!
//! `generate_test_certificate` trusts the generated certificate directly
//! as a client-side root. Certificate *validation* itself is not
//! bypassed: the client's `rustls::RootCertStore` only contains this one
//! certificate, so the normal X.509 path-building/signature/validity/
//! hostname checks in `rustls-webpki` still run against it during the
//! handshake, they just succeed because the presented leaf equals the
//! trusted root (a standard self-signed-cert-as-root test setup, the same
//! pattern quinn's own test suite uses).

use std::io;
use std::path::Path;
use std::sync::Arc;

use quinn::rustls::RootCertStore;
use quinn::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

/// A certificate/key pair plus the DER bytes a client needs to trust it.
/// Despite the name (kept for compatibility with this crate's existing
/// test suite), this is also the general-purpose holder produced by
/// [`load_certificate_files`] for real, non-test certificates.
pub struct TestCertificate {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivatePkcs8KeyDer<'static>,
}

/// Generates a fresh self-signed certificate valid for `hostname` (e.g.
/// `"localhost"`). **Test-only**: not a real CA, not for production use.
pub fn generate_test_certificate(hostname: &str) -> TestCertificate {
    generate_test_certificate_with_pem(hostname).0
}

/// Like [`generate_test_certificate`], but also returns the certificate's
/// PEM encoding. Used by `sardp-server`'s no-`--cert`-given convenience
/// path: a fresh self-signed cert generated per run (not a fixed
/// embedded one -- unlike the PoC's fixed auth keypair, baking a private
/// key into source control is bad practice regardless of PoC status) is
/// written out as PEM so the operator can point `sardp-client
/// --trust-cert` at it.
pub fn generate_test_certificate_with_pem(hostname: &str) -> (TestCertificate, String) {
    let certified_key = rcgen::generate_simple_self_signed(vec![hostname.to_string()])
        .expect("self-signed cert generation cannot fail for a valid hostname");
    let cert_pem = certified_key.cert.pem();
    let key_der = PrivatePkcs8KeyDer::from(certified_key.signing_key.serialize_der());
    let cert_der = CertificateDer::from(certified_key.cert);
    (TestCertificate { cert_der, key_der }, cert_pem)
}

/// Loads a real certificate/key pair from PEM files. The key MUST be
/// PKCS8 (`-----BEGIN PRIVATE KEY-----`, the default output of `openssl
/// genpkey`, `mkcert`, and this module's own `generate_test_certificate`);
/// older "traditional" PEM formats (`BEGIN RSA/EC PRIVATE KEY`) are not
/// recognized by `rustls_pemfile::pkcs8_private_keys` and will surface as
/// "no PKCS8 private key found".
pub fn load_certificate_files(cert_path: &Path, key_path: &Path) -> io::Result<TestCertificate> {
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;

    let cert_der = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no certificate found in {}", cert_path.display()),
            )
        })??;

    let key_der = rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_slice())
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "no PKCS8 private key found in {} (must be \"BEGIN PRIVATE KEY\", not \"BEGIN RSA/EC PRIVATE KEY\")",
                    key_path.display()
                ),
            )
        })??;

    Ok(TestCertificate { cert_der, key_der })
}

/// Loads a single certificate from a PEM file, for a client's trust root
/// (`sardp-client --trust-cert`; no private key needed). See
/// [`load_certificate_files`] for the server-side cert+key pair loader.
pub fn load_trusted_cert_pem(path: &Path) -> io::Result<CertificateDer<'static>> {
    let pem = std::fs::read(path)?;
    rustls_pemfile::certs(&mut pem.as_slice())
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no certificate found in {}", path.display()),
            )
        })?
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

    #[test]
    fn load_certificate_files_round_trips_a_pkcs8_pem_pair() {
        let certified_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("self-signed cert generation cannot fail for a valid hostname");
        let cert_pem = certified_key.cert.pem();
        let key_pem = certified_key.signing_key.serialize_pem();

        let dir = std::env::temp_dir().join(format!(
            "sardp-pki-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();

        let loaded =
            load_certificate_files(&cert_path, &key_path).expect("loads the PEM pair back");
        assert!(!loaded.cert_der.is_empty());
        assert!(!loaded.key_der.secret_pkcs8_der().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_certificate_files_reports_a_missing_file() {
        let result = load_certificate_files(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        );
        assert!(result.is_err());
    }
}
