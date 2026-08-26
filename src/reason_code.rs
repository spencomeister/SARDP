//! `ReasonCode` (spec 2.8): `{ domain: u8, code: u16 }`.
//!
//! v0.3 Part 4.8's Error Handling Matrix assigns explicit numeric `code`
//! values to some symbolic names (e.g. `AUTH.3 SIGNATURE_INVALID`), but not
//! to others referenced only symbolically in Part 4.1/4.7 (e.g.
//! `TRANSPORT.HANDSHAKE_TIMEOUT`, `AUTH.AUTH_TIMEOUT`). For M2 this module
//! reuses the numbers 4.8 gives where they exist, and assigns its own
//! sequential numbers, clearly marked, for the ones 4.8 leaves
//! unallocated. A future spec revision that allocates official numbers for
//! these should supersede the marked constants.

/// `ReasonCode.domain` values (spec 2.8).
pub mod domain {
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

    // -- Numbered by spec 4.8 Error Handling Matrix --
    pub const AUTH_TOO_MANY_ATTEMPTS: Self = Self::new(domain::AUTH, 2);
    pub const AUTH_SIGNATURE_INVALID: Self = Self::new(domain::AUTH, 3);
    pub const AUTH_CHALLENGE_REUSE: Self = Self::new(domain::AUTH, 4);
    pub const PROTOCOL_UNEXPECTED_MESSAGE: Self = Self::new(domain::PROTOCOL, 1);
    pub const PROTOCOL_UNKNOWN_CORE_MESSAGE: Self = Self::new(domain::PROTOCOL, 2);
    pub const PROTOCOL_PROLOGUE_MAGIC_MISMATCH: Self = Self::new(domain::PROTOCOL, 3);
    pub const PROTOCOL_UNKNOWN_STREAM_KIND: Self = Self::new(domain::PROTOCOL, 4);
    pub const PROTOCOL_WRONG_INITIATOR: Self = Self::new(domain::PROTOCOL, 5);

    // -- Referenced only symbolically in 4.1/4.7; not yet numbered in 4.8.
    //    Implementation-assigned pending an official allocation. --
    pub const TRANSPORT_HANDSHAKE_TIMEOUT: Self = Self::new(domain::TRANSPORT, 1);
    pub const AUTH_AUTH_TIMEOUT: Self = Self::new(domain::AUTH, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_match_spec_2_8() {
        assert_eq!(ReasonCode::AUTH_SIGNATURE_INVALID.domain, domain::AUTH);
        assert_eq!(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE.domain,
            domain::PROTOCOL
        );
    }

    #[test]
    fn codes_match_spec_4_8_table() {
        assert_eq!(ReasonCode::AUTH_TOO_MANY_ATTEMPTS.code, 2);
        assert_eq!(ReasonCode::AUTH_SIGNATURE_INVALID.code, 3);
        assert_eq!(ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE.code, 1);
    }
}
