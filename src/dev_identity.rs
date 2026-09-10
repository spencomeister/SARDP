//! A single fixed Ed25519 keypair, baked into source, so `sardp-server`
//! and `sardp-client` interoperate with zero configuration (PoC brief:
//! "認証は固定鍵ペアでの署名検証のみで構いません"). **This key is public**
//! (anyone with the source can sign as this identity) and MUST NOT be
//! used for anything beyond this PoC's own demo; a real deployment
//! provisions a per-user/per-device key out of band (out of scope, spec
//! Part 3). Override via `--signing-key-seed`/`--trusted-pubkey` on
//! either binary for anything beyond a local demo.
//!
//! This is unrelated to, and much lower-stakes than, the TLS certificate
//! `sardp-server` presents (see `pki::generate_test_certificate_with_pem`):
//! a leaked TLS private key compromises transport confidentiality for
//! anyone who can intercept traffic, whereas this key only lets someone
//! authenticate *as the PoC's one demo user* to a server that itself
//! chose to trust it (and real deployments override it).

use ed25519_dalek::{SigningKey, VerifyingKey};

/// Arbitrary fixed PoC-only seed. Not a secret in any meaningful sense --
/// see the module docs.
pub const DEV_SIGNING_KEY_SEED: [u8; 32] = [0x5A; 32];

pub fn dev_signing_key() -> SigningKey {
    SigningKey::from_bytes(&DEV_SIGNING_KEY_SEED)
}

pub fn dev_verifying_key() -> VerifyingKey {
    dev_signing_key().verifying_key()
}

/// Parses a 64-character hex string into 32 bytes (an Ed25519 seed or
/// public key). Used by both binaries' `--signing-key-seed`/
/// `--trusted-pubkey` flags.
pub fn parse_hex32(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!(
            "expected 64 hex characters (32 bytes), got {} characters",
            s.len()
        ));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("invalid hex at byte {i}: {e}"))?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_keys_are_a_matching_pair() {
        let signing_key = dev_signing_key();
        assert_eq!(signing_key.verifying_key(), dev_verifying_key());
    }

    #[test]
    fn parse_hex32_round_trips() {
        let bytes = [0xAB; 32];
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(parse_hex32(&hex).unwrap(), bytes);
    }

    #[test]
    fn parse_hex32_rejects_wrong_length() {
        assert!(parse_hex32("abcd").is_err());
    }

    #[test]
    fn parse_hex32_rejects_non_hex() {
        let bad = "zz".repeat(32);
        assert!(parse_hex32(&bad).is_err());
    }
}
