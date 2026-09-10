//! `ReasonCode` (spec 2.8): `{ domain: u8, code: u16 }`.
//!
//! Numbers below come from the canonical table in spec Part 4.8.1
//! ("ReasonCode 一覧(正式版、DR-034)"), which is the single source of
//! truth for `domain.code` assignments as of this revision. `domain = 0,
//! code = 0` is the reserved "no error" value (DR-034): messages with a
//! mandatory `ReasonCode` field that also carry a success case (e.g.
//! `AuthResult{status: Ok}`) MUST set it there.

/// `ReasonCode.domain` values (spec 2.8, DR-034).
pub mod domain {
    /// Reserved "no error" domain (DR-034); only valid paired with `code = 0`.
    pub const NONE: u8 = 0;
    pub const AUTH: u8 = 1;
    pub const POLICY: u8 = 2;
    pub const TRANSPORT: u8 = 3;
    pub const PROTOCOL: u8 = 4;
    pub const OS: u8 = 5;
}

/// `ReasonCode { domain, code }` (spec 2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReasonCode {
    pub domain: u8,
    pub code: u16,
}

impl ReasonCode {
    const fn new(domain: u8, code: u16) -> Self {
        Self { domain, code }
    }

    /// `domain=0, code=0`: "no error" (spec 2.8, DR-034). MUST be used for
    /// the `reason` field of otherwise-successful messages such as
    /// `AuthResult{status: Ok}`.
    pub const NONE: Self = Self::new(domain::NONE, 0);

    // -- AUTH (domain=1) --
    pub const AUTH_TIMEOUT: Self = Self::new(domain::AUTH, 1);
    pub const AUTH_TOO_MANY_ATTEMPTS: Self = Self::new(domain::AUTH, 2);
    pub const AUTH_SIGNATURE_INVALID: Self = Self::new(domain::AUTH, 3);
    pub const AUTH_CHALLENGE_REUSE: Self = Self::new(domain::AUTH, 4);
    pub const AUTH_RECONNECT_TOKEN_INVALID: Self = Self::new(domain::AUTH, 5);
    pub const AUTH_RECONNECT_TOKEN_EXPIRED: Self = Self::new(domain::AUTH, 6);
    pub const AUTH_RECONNECT_TOKEN_ALREADY_CONSUMED: Self = Self::new(domain::AUTH, 7);

    // -- POLICY (domain=2) --
    pub const POLICY_PERMISSION_DENIED: Self = Self::new(domain::POLICY, 1);
    pub const POLICY_PERMISSION_REVOKED: Self = Self::new(domain::POLICY, 2);
    pub const POLICY_FORCED_DISCONNECT: Self = Self::new(domain::POLICY, 3);
    pub const POLICY_MAX_SESSION_DURATION_EXCEEDED: Self = Self::new(domain::POLICY, 4);
    pub const POLICY_FILE_POLICY_REJECTED: Self = Self::new(domain::POLICY, 5);
    pub const POLICY_CLIPBOARD_FORMAT_TOO_LARGE: Self = Self::new(domain::POLICY, 6);

    // -- TRANSPORT (domain=3) --
    pub const TRANSPORT_HANDSHAKE_TIMEOUT: Self = Self::new(domain::TRANSPORT, 1);
    pub const TRANSPORT_IDLE_TIMEOUT: Self = Self::new(domain::TRANSPORT, 2);
    pub const TRANSPORT_RECONNECT_TIMEOUT: Self = Self::new(domain::TRANSPORT, 3);
    pub const TRANSPORT_VIDEO_RECOVERY_TIMEOUT: Self = Self::new(domain::TRANSPORT, 4);
    pub const TRANSPORT_STREAM_STALL_TIMEOUT: Self = Self::new(domain::TRANSPORT, 5);

    // -- PROTOCOL (domain=4) --
    pub const PROTOCOL_UNEXPECTED_MESSAGE: Self = Self::new(domain::PROTOCOL, 1);
    pub const PROTOCOL_UNKNOWN_CORE_MESSAGE: Self = Self::new(domain::PROTOCOL, 2);
    pub const PROTOCOL_PROLOGUE_MAGIC_MISMATCH: Self = Self::new(domain::PROTOCOL, 3);
    pub const PROTOCOL_UNKNOWN_STREAM_KIND: Self = Self::new(domain::PROTOCOL, 4);
    pub const PROTOCOL_WRONG_INITIATOR: Self = Self::new(domain::PROTOCOL, 5);
    pub const PROTOCOL_SESSION_SETUP_TIMEOUT: Self = Self::new(domain::PROTOCOL, 6);
    pub const PROTOCOL_VIDEO_CONFIGURING_TIMEOUT: Self = Self::new(domain::PROTOCOL, 7);
    pub const PROTOCOL_FRAME_LENGTH_MISMATCH: Self = Self::new(domain::PROTOCOL, 8);
    pub const PROTOCOL_GENERATION_MISMATCH: Self = Self::new(domain::PROTOCOL, 9);
    pub const PROTOCOL_FILE_CHUNK_OVERLAP: Self = Self::new(domain::PROTOCOL, 10);
    pub const PROTOCOL_FILE_CHUNK_OUT_OF_RANGE: Self = Self::new(domain::PROTOCOL, 11);
    pub const PROTOCOL_FILE_INCOMPLETE_TRANSFER: Self = Self::new(domain::PROTOCOL, 12);
    pub const PROTOCOL_FILE_CHECKSUM_MISMATCH: Self = Self::new(domain::PROTOCOL, 13);

    // -- OS (domain=5) --
    // code 1 (CAPTURE_FAILURE) and 3 (INPUT_INJECTION_FAILURE) are
    // reserved but unassigned in spec 4.8.1; only 2 is defined.
    pub const OS_DECODE_ERROR: Self = Self::new(domain::OS, 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_domain_and_code_zero() {
        assert_eq!(ReasonCode::NONE, ReasonCode { domain: 0, code: 0 });
    }

    #[test]
    fn domains_match_spec_2_8() {
        assert_eq!(ReasonCode::AUTH_SIGNATURE_INVALID.domain, domain::AUTH);
        assert_eq!(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE.domain,
            domain::PROTOCOL
        );
        assert_eq!(ReasonCode::POLICY_PERMISSION_DENIED.domain, domain::POLICY);
        assert_eq!(ReasonCode::TRANSPORT_IDLE_TIMEOUT.domain, domain::TRANSPORT);
        assert_eq!(ReasonCode::OS_DECODE_ERROR.domain, domain::OS);
    }

    #[test]
    fn codes_match_spec_4_8_1_table() {
        assert_eq!(ReasonCode::AUTH_TIMEOUT.code, 1);
        assert_eq!(ReasonCode::AUTH_TOO_MANY_ATTEMPTS.code, 2);
        assert_eq!(ReasonCode::AUTH_SIGNATURE_INVALID.code, 3);
        assert_eq!(ReasonCode::AUTH_CHALLENGE_REUSE.code, 4);
        assert_eq!(ReasonCode::AUTH_RECONNECT_TOKEN_INVALID.code, 5);
        assert_eq!(ReasonCode::AUTH_RECONNECT_TOKEN_EXPIRED.code, 6);
        assert_eq!(ReasonCode::AUTH_RECONNECT_TOKEN_ALREADY_CONSUMED.code, 7);

        assert_eq!(ReasonCode::TRANSPORT_HANDSHAKE_TIMEOUT.code, 1);
        assert_eq!(ReasonCode::TRANSPORT_IDLE_TIMEOUT.code, 2);
        assert_eq!(ReasonCode::TRANSPORT_RECONNECT_TIMEOUT.code, 3);
        assert_eq!(ReasonCode::TRANSPORT_VIDEO_RECOVERY_TIMEOUT.code, 4);
        assert_eq!(ReasonCode::TRANSPORT_STREAM_STALL_TIMEOUT.code, 5);

        assert_eq!(ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE.code, 1);
        assert_eq!(ReasonCode::PROTOCOL_VIDEO_CONFIGURING_TIMEOUT.code, 7);
        assert_eq!(ReasonCode::PROTOCOL_FRAME_LENGTH_MISMATCH.code, 8);
        assert_eq!(ReasonCode::PROTOCOL_GENERATION_MISMATCH.code, 9);

        assert_eq!(ReasonCode::OS_DECODE_ERROR.code, 2);
    }
}
