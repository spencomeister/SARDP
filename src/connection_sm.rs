//! Connection State Machine (spec 4.1), scoped to the M2 milestone:
//! `Handshaking -> Authenticating -> Authenticated`. Later milestones add
//! `Active`, `Suspended`, `Closed` and the fuller message catalog that
//! governs legality once `Authenticated`.
//!
//! Per-state message legality (the "許可されるメッセージ" column of the
//! 4.1 table) is enforced here; wall-clock timeouts (`HANDSHAKE_TIMEOUT`,
//! `AUTH_TIMEOUT`) are enforced by the caller wrapping the relevant phase
//! in `tokio::time::timeout` rather than by this type, since that avoids
//! this pure state machine needing its own clock.

use crate::messages::type_id;
use crate::reason_code::ReasonCode;

/// Spec 2.3 / 4.7: connection-level authentication attempt limit.
pub const AUTH_ATTEMPT_LIMIT: u8 = 3;

/// Connection SM states relevant through M2 (spec 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Handshaking,
    Authenticating,
    Authenticated,
    Closing(ReasonCode),
}

/// A message, or transition request, that is illegal in the current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolViolation {
    pub reason: ReasonCode,
}

/// Drives the M2 subset of the Connection SM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSm {
    state: ConnectionState,
    auth_attempts: u8,
}

impl Default for ConnectionSm {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionSm {
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Handshaking,
            auth_attempts: 0,
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Spec 4.1 Handshaking row: "`ClientHello`、`ServerHello` のみ".
    fn allowed_in_handshaking(type_id: u16) -> bool {
        matches!(type_id, type_id::CLIENT_HELLO | type_id::SERVER_HELLO)
    }

    /// Spec 4.1 Authenticating row, restricted to the M2 subset
    /// (`AuthPasskeyAssertion`/`AuthChallengeRenew` are out of scope).
    fn allowed_in_authenticating(type_id: u16) -> bool {
        type_id == type_id::AUTH_PUBKEY
    }

    /// Checks whether an incoming control message with this `type_id` is
    /// legal in the current state (spec 4.1's per-state message column).
    /// Does not itself change state.
    pub fn check_message(&self, type_id: u16) -> Result<(), ProtocolViolation> {
        let allowed = match self.state {
            ConnectionState::Handshaking => Self::allowed_in_handshaking(type_id),
            ConnectionState::Authenticating => Self::allowed_in_authenticating(type_id),
            // Authenticated's full message catalog (DisplayConfig, video
            // stream openings, ...) arrives with later milestones; nothing
            // to reject against yet.
            ConnectionState::Authenticated => true,
            ConnectionState::Closing(_) => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            })
        }
    }

    /// `Handshaking -> Authenticating`, once ClientHello/ServerHello have
    /// been exchanged (spec 4.1).
    pub fn complete_handshake(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            ConnectionState::Handshaking => {
                self.state = ConnectionState::Authenticating;
                Ok(())
            }
            _ => Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            }),
        }
    }

    /// Records a failed authentication attempt (e.g. invalid signature).
    /// Transitions to `Closing` once [`AUTH_ATTEMPT_LIMIT`] is reached
    /// (spec 2.3, 4.1). Always returns `Err`: this method exists to react
    /// to a failure that already happened, not to succeed.
    pub fn record_auth_failure(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            ConnectionState::Authenticating => {
                self.auth_attempts += 1;
                if self.auth_attempts >= AUTH_ATTEMPT_LIMIT {
                    self.state = ConnectionState::Closing(ReasonCode::AUTH_TOO_MANY_ATTEMPTS);
                    Err(ProtocolViolation {
                        reason: ReasonCode::AUTH_TOO_MANY_ATTEMPTS,
                    })
                } else {
                    Err(ProtocolViolation {
                        reason: ReasonCode::AUTH_SIGNATURE_INVALID,
                    })
                }
            }
            _ => Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            }),
        }
    }

    /// `Authenticating -> Authenticated` on `AuthResult{status=OK}` (spec 4.1).
    pub fn complete_authentication(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            ConnectionState::Authenticating => {
                self.state = ConnectionState::Authenticated;
                Ok(())
            }
            _ => Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_in_handshaking() {
        assert_eq!(ConnectionSm::new().state(), ConnectionState::Handshaking);
    }

    #[test]
    fn handshaking_allows_hello_messages() {
        let sm = ConnectionSm::new();
        assert_eq!(sm.check_message(type_id::CLIENT_HELLO), Ok(()));
        assert_eq!(sm.check_message(type_id::SERVER_HELLO), Ok(()));
    }

    #[test]
    fn handshaking_rejects_auth_pubkey() {
        let sm = ConnectionSm::new();
        assert_eq!(
            sm.check_message(type_id::AUTH_PUBKEY),
            Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
            })
        );
    }

    #[test]
    fn complete_handshake_transitions_to_authenticating() {
        let mut sm = ConnectionSm::new();
        assert_eq!(sm.complete_handshake(), Ok(()));
        assert_eq!(sm.state(), ConnectionState::Authenticating);
    }

    #[test]
    fn complete_handshake_twice_is_rejected() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        assert_eq!(
            sm.complete_handshake(),
            Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
            })
        );
    }

    #[test]
    fn authenticating_allows_only_auth_pubkey() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        assert_eq!(sm.check_message(type_id::AUTH_PUBKEY), Ok(()));
        assert!(sm.check_message(type_id::CLIENT_HELLO).is_err());
        assert!(sm.check_message(type_id::SERVER_HELLO).is_err());
    }

    #[test]
    fn auth_failure_in_wrong_state_is_unexpected_message() {
        let mut sm = ConnectionSm::new();
        assert_eq!(
            sm.record_auth_failure(),
            Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
            })
        );
    }

    #[test]
    fn auth_failure_below_limit_stays_authenticating() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        assert_eq!(
            sm.record_auth_failure(),
            Err(ProtocolViolation {
                reason: ReasonCode::AUTH_SIGNATURE_INVALID
            })
        );
        assert_eq!(sm.state(), ConnectionState::Authenticating);
        assert_eq!(
            sm.record_auth_failure(),
            Err(ProtocolViolation {
                reason: ReasonCode::AUTH_SIGNATURE_INVALID
            })
        );
        assert_eq!(sm.state(), ConnectionState::Authenticating);
    }

    #[test]
    fn third_auth_failure_closes_connection() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.record_auth_failure().unwrap_err();
        sm.record_auth_failure().unwrap_err();
        assert_eq!(
            sm.record_auth_failure(),
            Err(ProtocolViolation {
                reason: ReasonCode::AUTH_TOO_MANY_ATTEMPTS
            })
        );
        assert_eq!(
            sm.state(),
            ConnectionState::Closing(ReasonCode::AUTH_TOO_MANY_ATTEMPTS)
        );
    }

    #[test]
    fn complete_authentication_transitions_to_authenticated() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        assert_eq!(sm.complete_authentication(), Ok(()));
        assert_eq!(sm.state(), ConnectionState::Authenticated);
    }

    #[test]
    fn complete_authentication_before_handshake_done_is_rejected() {
        let mut sm = ConnectionSm::new();
        assert!(sm.complete_authentication().is_err());
        assert_eq!(sm.state(), ConnectionState::Handshaking);
    }

    #[test]
    fn full_happy_path_with_retries() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.record_auth_failure().unwrap_err(); // 1st bad signature
        sm.complete_authentication().unwrap(); // then a good one succeeds
        assert_eq!(sm.state(), ConnectionState::Authenticated);
    }

    #[test]
    fn closing_rejects_all_messages() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.record_auth_failure().unwrap_err();
        sm.record_auth_failure().unwrap_err();
        sm.record_auth_failure().unwrap_err();
        assert!(matches!(sm.state(), ConnectionState::Closing(_)));
        assert!(sm.check_message(type_id::AUTH_PUBKEY).is_err());
    }
}
