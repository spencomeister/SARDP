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
//! M3 adds the video setup/frame messages: `VideoStreamGeneration`,
//! `EncoderConfig`, `VideoFrameHeader` (spec 2.10).
//!
//! v0.3 does not assign numeric `Envelope.type` ids to individual control
//! messages anywhere; [`type_id`] is this implementation's own core-range
//! (DR-014) assignment, not a spec-mandated value.
//!
//! Per DR-035, the logical `VideoFrame` is split on the wire into two
//! consecutive Envelopes: [`VideoFrameHeader`] (CBOR, this module) and a
//! raw-bytes `VideoFramePayload` (not a struct here at all -- it's just
//! the H.264 Annex-B bytes handed directly to `Envelope::encode`, with no
//! schema wrapping). This resolves the M3 ambiguity between DR-021's
//! schema-encoded "メッセージ本体" category and its unwrapped-raw-bytes
//! "映像...のペイロード" category: cramming both into one CBOR struct (M3's
//! original approach) forced a copy into a schema-allocated buffer for
//! the payload field, defeating the zero-copy intent; two Envelopes let
//! the payload ride entirely inside the Envelope layer's own raw-bytes
//! handling (spec 2.1.1). See `video_session` for the send/receive
//! sequencing (`VideoFramePayload` MUST immediately follow
//! `VideoFrameHeader`, spec 2.10).

use serde::{Deserialize, Serialize};

use crate::reason_code::ReasonCode;

/// Implementation-assigned `Envelope.type` ids for control messages
/// (core range, DR-014). Not specified numerically in v0.3.
pub mod type_id {
    pub const CLIENT_HELLO: u16 = 0x0001;
    pub const SERVER_HELLO: u16 = 0x0002;
    pub const AUTH_PUBKEY: u16 = 0x0003;
    pub const AUTH_RESULT: u16 = 0x0004;
    pub const VIDEO_STREAM_GENERATION: u16 = 0x0005;
    pub const ENCODER_CONFIG: u16 = 0x0006;
    pub const VIDEO_FRAME_HEADER: u16 = 0x0007;
    /// Raw H.264 Annex-B bytes; not CBOR-encoded (DR-035).
    pub const VIDEO_FRAME_PAYLOAD: u16 = 0x0008;
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
    #[serde(with = "serde_bytes")]
    pub public_key: Vec<u8>,
    #[serde(with = "serde_bytes")]
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

/// `EncoderConfig.codec` enum (spec 2.10). Only `H264` is defined in v0.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
}

/// `EncoderConfig.chroma_format` enum (spec 2.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromaFormat {
    C420,
    C422,
    C444,
}

/// `VideoStreamGeneration` (spec 2.10, video, server->client). First
/// message sent on a newly opened video Instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoStreamGeneration {
    /// This stream Instance's generation number; 0-based, +1 on every
    /// reopen (spec 2.10, DR-017).
    pub generation: u64,
    /// References `DisplayConfig.config_id`.
    pub config_id: u64,
}

/// `EncoderConfig` (spec 2.10, video, server->client). Sent immediately
/// after `VideoStreamGeneration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub profile: u16,
    pub chroma_format: ChromaFormat,
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    pub max_fps: u16,
    pub tier: u8,
    /// MUST be 0 in v0.3 (B-frames prohibited, DR-019).
    pub b_frames: u8,
    pub server_cursor_excludable: bool,
}

/// `VideoFrameHeader` (spec 2.10, DR-035): the CBOR half of the logical
/// `VideoFrame`. Always immediately followed, on the same stream, by a
/// raw-bytes `VideoFramePayload` Envelope (`type_id::VIDEO_FRAME_PAYLOAD`)
/// carrying the H.264 Annex-B bytes -- see `video_session` for the
/// send/receive sequencing and the `payload_len` cross-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrameHeader {
    pub generation: u64,
    /// 0-based within `generation`; send order = decode order = display
    /// order since B-frames are prohibited (spec 2.10, DR-019).
    pub frame_id: u64,
    pub config_id: u64,
    /// Bit 0: IDR.
    pub flags: u8,
    pub capture_ts: u64,
    pub encode_done_ts: u64,
    /// Required only on a resolution change (spec 2.10); this PoC always
    /// sets it, matching `EncoderConfig.width/height`.
    pub width: u32,
    pub height: u32,
    /// MUST equal the immediately-following `VideoFramePayload`
    /// Envelope's `length` (spec 2.10, 4.8 `PROTOCOL.8
    /// FRAME_LENGTH_MISMATCH`).
    pub payload_len: u64,
}

/// `VideoFrameHeader.flags` bit 0 (spec 2.10).
pub const VIDEO_FRAME_FLAG_IDR: u8 = 0x01;

impl VideoFrameHeader {
    pub fn is_idr(&self) -> bool {
        self.flags & VIDEO_FRAME_FLAG_IDR != 0
    }
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
    fn video_stream_generation_round_trips() {
        let msg = VideoStreamGeneration {
            generation: 3,
            config_id: 42,
        };
        let bytes = encode(&msg);
        let decoded: VideoStreamGeneration = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encoder_config_round_trips() {
        let msg = EncoderConfig {
            codec: Codec::H264,
            profile: 66,
            chroma_format: ChromaFormat::C420,
            bit_depth: 8,
            width: 1920,
            height: 1080,
            max_fps: 60,
            tier: 1,
            b_frames: 0,
            server_cursor_excludable: true,
        };
        let bytes = encode(&msg);
        let decoded: EncoderConfig = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn video_frame_header_round_trips() {
        let msg = VideoFrameHeader {
            generation: 0,
            frame_id: 0,
            config_id: 1,
            flags: VIDEO_FRAME_FLAG_IDR,
            capture_ts: 1_000_000,
            encode_done_ts: 1_000_500,
            width: 1920,
            height: 1080,
            payload_len: 7,
        };
        let bytes = encode(&msg);
        let decoded: VideoFrameHeader = decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
        assert!(decoded.is_idr());
    }

    #[test]
    fn video_frame_header_is_compact() {
        // Sanity check that the header alone (no payload bytes attached)
        // stays small regardless of how large the frame it describes is
        // -- the whole point of separating it from the payload (DR-035).
        let msg = VideoFrameHeader {
            generation: 0,
            frame_id: 0,
            config_id: 0,
            flags: 0,
            capture_ts: 0,
            encode_done_ts: 0,
            width: 0,
            height: 0,
            payload_len: 8_000_000, // an 8 MiB frame, at the video stream limit
        };
        let bytes = encode(&msg);
        assert!(
            bytes.len() < 200,
            "header should be tiny regardless of payload_len, got {}",
            bytes.len()
        );
    }

    #[test]
    fn is_idr_checks_only_bit_0() {
        let mut msg = VideoFrameHeader {
            generation: 0,
            frame_id: 1,
            config_id: 0,
            flags: 0b0000_0010, // some other flag bit set, not IDR
            capture_ts: 0,
            encode_done_ts: 0,
            width: 0,
            height: 0,
            payload_len: 0,
        };
        assert!(!msg.is_idr());
        msg.flags |= VIDEO_FRAME_FLAG_IDR;
        assert!(msg.is_idr());
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
