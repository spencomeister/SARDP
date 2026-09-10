//! Authentication channel binding (spec 2.3, DR-005) and signature
//! verification, scoped to the PoC brief's simplification: a single fixed
//! Ed25519 test keypair stands in for WebAuthn/Passkey enrollment, and
//! `PUBLIC_KEY` is the only `AuthMethod` this implementation exercises.
//!
//! ```text
//! context  = "SARDP-auth-v1" || hash(ClientHello) || hash(ServerHello) || auth_challenge
//! exporter = TLS-Exporter(label="EXPORTER-SARDP-auth-v1", context=context, length=32)
//! signature の対象 = exporter
//! ```
//!
//! Spec 2.3 does not name a hash algorithm for `hash(ClientHello)` /
//! `hash(ServerHello)`; this implementation uses SHA-256 over each
//! message's CBOR-encoded Envelope payload bytes, consistent with the
//! SHA-256 already used elsewhere on the wire (`FileTransferComplete`,
//! spec 2.6).
//!
//! The TLS exporter itself is `quinn::Connection::export_keying_material`,
//! which forwards to rustls's RFC 5705 / TLS 1.3 exporter implementation —
//! confirmed working end-to-end in `tests/m2_handshake.rs` using a
//! loopback QUIC connection with an `rcgen` test CA. No stub was needed.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// Channel-binding context prefix (spec 2.3).
pub const CONTEXT_PREFIX: &[u8] = b"SARDP-auth-v1";
/// TLS exporter label (spec 2.3).
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-SARDP-auth-v1";
/// TLS exporter output length in bytes (spec 2.3: `length=32`).
pub const EXPORTER_LENGTH: usize = 32;

/// A fixed Ed25519 test keypair seed, standing in for real device
/// enrollment per the PoC brief ("固定の鍵ペア"). **Test-only**: anyone
/// with this source file can derive the private key.
pub const TEST_SIGNING_KEY_SEED: [u8; 32] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
];

/// Derives the fixed PoC test signing key from [`TEST_SIGNING_KEY_SEED`].
pub fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&TEST_SIGNING_KEY_SEED)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Builds the channel-binding `context` bytes (spec 2.3).
///
/// `client_hello_bytes` / `server_hello_bytes` are the exact bytes hashed
/// into the binding: this implementation hashes each message's
/// CBOR-encoded Envelope *payload* (i.e. `messages::encode(&hello)`, not
/// the framed Envelope with its length/type header).
pub fn channel_binding_context(
    client_hello_bytes: &[u8],
    server_hello_bytes: &[u8],
    auth_challenge: &[u8; 32],
) -> Vec<u8> {
    let mut ctx = Vec::with_capacity(CONTEXT_PREFIX.len() + 32 + 32 + auth_challenge.len());
    ctx.extend_from_slice(CONTEXT_PREFIX);
    ctx.extend_from_slice(&sha256(client_hello_bytes));
    ctx.extend_from_slice(&sha256(server_hello_bytes));
    ctx.extend_from_slice(auth_challenge);
    ctx
}

/// Authentication failures. Deliberately coarse-grained: spec 4.8 maps all
/// of these to `AUTH.3 SIGNATURE_INVALID` except where noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// The QUIC/TLS layer could not produce exporter material (e.g. called
    /// before the handshake completed).
    ExportKeyingMaterialFailed,
    /// Malformed key/signature bytes, or a cryptographically invalid
    /// signature.
    SignatureInvalid,
}

/// Derives the channel-binding exporter value from a live QUIC connection.
///
/// Both peers, calling this with the same `context` on their own
/// [`quinn::Connection`], get identical output (RFC 5705) because it is
/// derived from the shared TLS session secrets.
pub fn compute_exporter(
    connection: &quinn::Connection,
    context: &[u8],
) -> Result<[u8; EXPORTER_LENGTH], AuthError> {
    let mut out = [0u8; EXPORTER_LENGTH];
    connection
        .export_keying_material(&mut out, EXPORTER_LABEL, context)
        .map_err(|_| AuthError::ExportKeyingMaterialFailed)?;
    Ok(out)
}

/// Signs the exporter value (the `AuthPubkey.signature` target, spec 2.3).
pub fn sign_exporter(signing_key: &SigningKey, exporter: &[u8; EXPORTER_LENGTH]) -> Vec<u8> {
    signing_key.sign(exporter).to_bytes().to_vec()
}

/// Verifies an `AuthPubkey.signature` over the exporter value using the
/// claimed `public_key` bytes.
///
/// This only checks cryptographic validity of the signature; it does not
/// check whether `public_key_bytes` is a key the server actually trusts
/// (that is a separate, policy-level check the caller must perform, e.g.
/// against the PoC's single hardcoded trusted key).
pub fn verify_exporter(
    public_key_bytes: &[u8],
    exporter: &[u8; EXPORTER_LENGTH],
    signature_bytes: &[u8],
) -> Result<(), AuthError> {
    let public_key_array: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| AuthError::SignatureInvalid)?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key_array).map_err(|_| AuthError::SignatureInvalid)?;
    let signature_array: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| AuthError::SignatureInvalid)?;
    let signature = Signature::from_bytes(&signature_array);
    verifying_key
        .verify(exporter, &signature)
        .map_err(|_| AuthError::SignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_succeeds() {
        let signing_key = test_signing_key();
        let verifying_key = signing_key.verifying_key();
        let exporter = [0x42; EXPORTER_LENGTH];

        let signature = sign_exporter(&signing_key, &exporter);

        assert_eq!(
            verify_exporter(verifying_key.as_bytes(), &exporter, &signature),
            Ok(())
        );
    }

    #[test]
    fn verify_rejects_wrong_exporter() {
        let signing_key = test_signing_key();
        let verifying_key = signing_key.verifying_key();
        let exporter = [0x42; EXPORTER_LENGTH];
        let signature = sign_exporter(&signing_key, &exporter);

        let wrong_exporter = [0x43; EXPORTER_LENGTH];
        assert_eq!(
            verify_exporter(verifying_key.as_bytes(), &wrong_exporter, &signature),
            Err(AuthError::SignatureInvalid)
        );
    }

    #[test]
    fn verify_rejects_wrong_public_key() {
        let signing_key = test_signing_key();
        let exporter = [0x42; EXPORTER_LENGTH];
        let signature = sign_exporter(&signing_key, &exporter);

        let other_key = SigningKey::from_bytes(&[0xAA; 32]);
        let wrong_verifying_key = other_key.verifying_key();
        assert_eq!(
            verify_exporter(wrong_verifying_key.as_bytes(), &exporter, &signature),
            Err(AuthError::SignatureInvalid)
        );
    }

    #[test]
    fn verify_rejects_malformed_public_key() {
        let exporter = [0x42; EXPORTER_LENGTH];
        assert_eq!(
            verify_exporter(&[1, 2, 3], &exporter, &[0u8; 64]),
            Err(AuthError::SignatureInvalid)
        );
    }

    #[test]
    fn verify_rejects_malformed_signature() {
        let signing_key = test_signing_key();
        let verifying_key = signing_key.verifying_key();
        let exporter = [0x42; EXPORTER_LENGTH];
        assert_eq!(
            verify_exporter(verifying_key.as_bytes(), &exporter, &[1, 2, 3]),
            Err(AuthError::SignatureInvalid)
        );
    }

    #[test]
    fn channel_binding_context_changes_with_any_input() {
        let base = channel_binding_context(b"client-hello", b"server-hello", &[0u8; 32]);
        let different_client =
            channel_binding_context(b"other-client-hello", b"server-hello", &[0u8; 32]);
        let different_server =
            channel_binding_context(b"client-hello", b"other-server-hello", &[0u8; 32]);
        let different_challenge =
            channel_binding_context(b"client-hello", b"server-hello", &[1u8; 32]);

        assert_ne!(base, different_client);
        assert_ne!(base, different_server);
        assert_ne!(base, different_challenge);
    }

    #[test]
    fn channel_binding_context_starts_with_prefix() {
        let ctx = channel_binding_context(b"a", b"b", &[0u8; 32]);
        assert!(ctx.starts_with(CONTEXT_PREFIX));
    }
}
