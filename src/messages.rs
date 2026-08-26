//! Control-stream message bodies (spec 2.3), CBOR-encoded per the
//! DR-021 policy: message bodies are schema-driven (this implementation
//! chose `serde` + CBOR, one of the DR-021-listed options), distinct from
//! the hand-written Envelope/StreamPrologue parsers in M1.
//!
//! Only the M2 subset is implemented: `ClientHello`, `ServerHello`,
//! `AuthPubkey`, `AuthResult`. `AuthChallengeRenew`, `AuthPasskeyAssertion`,
//! and `SessionReauthenticate` are out of scope for M2 (challenge renewal
//! and retry limits are explicitly optional in the PoC brief).
//!
//! v0.3 does not assign numeric `Envelope.type` ids to individual control
//! messages anywhere; [`type_id`] is this implementation's own core-range
//! (DR-014) assignment, not a spec-mandated value.

use serde::{Deserialize, Serialize};

use crate::reason_code::ReasonCode;

/// Implementation-assigned `Envelope.type` ids for control messages
/// (core range, DR-014). Not specified numerically in v0.3.
pub mod type_id {
    pub const CLIENT_HELLO: u16 = 0x0001;
    pub const SERVER_HELLO: u16 = 0x0002;
    pub const AUTH_PUBKEY: u16 = 0x0003;
    pub const AUTH_RESULT: u16 = 0x0004;
}

/// `AuthMethod` enum (spec 2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    PublicKey,
    Passkey,
    Password,
    Totp,
}

/// `AuthCombination` (spec 2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCombination {
    pub methods: Vec<AuthMethod>,
    pub priority: u8,
}

/// `AuthPolicy` (spec 2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPolicy {
    pub accepted_combinations: Vec<AuthCombination>,
}

/// `ClientHello` (spec 2.3, control, client->server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub client_name: String,
    pub client_version: String,
    /// `CapabilityId` is not concretely typed anywhere in v0.3; this PoC
    /// carries it as an opaque u16 and always sends an empty list.
    pub capabilities: Vec<u16>,
    pub auth_methods: Vec<AuthMethod>,
}

/// `ServerHello` (spec 2.3, control, server->client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub server_name: String,
    pub server_version: String,
    pub capabilities: Vec<u16>,
    pub auth_policy: AuthPolicy,
    /// One-time challenge; MUST NOT be resent or reused (spec 2.3).
    pub auth_challenge: [u8; 32],
}

/// `AuthPubkey` (spec 2.3, control, client->server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPubkey {
    pub user_id: String,
    pub device_id: String,
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

/// `AuthResult.status` enum (spec 2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthStatus {
    Ok,
    MfaRequired,
    Denied,
}

/// `AuthResult` (spec 2.3, control, server->client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResult {
    pub status: AuthStatus,
    pub reason: ReasonCode,
    pub session_id: [u8; 16],
    pub reconnect_token: [u8; 32],
    /// `PermissionSet` bitflags (spec 2.5); not defined further for M2.
    pub granted_permissions: u32,
}

/// CBOR-encodes `msg` (the DR-021 message-body scheme for this
/// implementation). Encoding an owned, in-memory `Vec<u8>` sink cannot
/// fail for any of the message types in this module.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(msg, &mut buf).expect("CBOR-encoding to a Vec<u8> cannot fail");
    buf
}

/// Decodes a CBOR-encoded message body of type `T` from an Envelope
/// payload. `ciborium` validates the CBOR structure against `T`'s schema
/// and never panics on malformed or attacker-controlled `bytes`.
pub fn decode<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, ciborium::de::Error<std::io::Error>> {
    ciborium::from_reader(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_round_trips() {
        let msg = ClientHello {
            client_name: "test-client".into(),
            client_version: "0.1.0".into(),
            capabilities: vec![],
            auth_methods: vec![AuthMethod::PublicKey],
        };
        let bytes = encode(&msg);
        let decoded: ClientHello = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn server_hello_round_trips() {
        let msg = ServerHello {
            server_name: "test-server".into(),
            server_version: "0.1.0".into(),
            capabilities: vec![],
            auth_policy: AuthPolicy {
                accepted_combinations: vec![AuthCombination {
                    methods: vec![AuthMethod::PublicKey],
                    priority: 0,
                }],
            },
            auth_challenge: [0x42; 32],
        };
        let bytes = encode(&msg);
        let decoded: ServerHello = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn auth_pubkey_round_trips() {
        let msg = AuthPubkey {
            user_id: "alice".into(),
            device_id: "device-1".into(),
            public_key: vec![1, 2, 3, 4],
            signature: vec![5; 64],
        };
        let bytes = encode(&msg);
        let decoded: AuthPubkey = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn auth_result_round_trips() {
        let msg = AuthResult {
            status: AuthStatus::Ok,
            reason: ReasonCode { domain: 0, code: 0 },
            session_id: [1; 16],
            reconnect_token: [2; 32],
            granted_permissions: 0xFF,
        };
        let bytes = encode(&msg);
        let decoded: AuthResult = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_rejects_garbage_bytes() {
        let result: Result<ClientHello, _> = decode(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_rejects_wrong_message_type() {
        let client_hello = ClientHello {
            client_name: "x".into(),
            client_version: "1".into(),
            capabilities: vec![],
            auth_methods: vec![],
        };
        let bytes = encode(&client_hello);
        let result: Result<AuthResult, _> = decode(&bytes);
        assert!(result.is_err());
    }
}
