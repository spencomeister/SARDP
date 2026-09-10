//! Connection State Machine (spec 4.1). Through M4:
//! `Handshaking -> Authenticating -> Authenticated -> Active`, the last
//! step on the first VideoStream Channel reaching `Live` (DR-024).
//! `Suspended`/reconnection and the fuller message catalog that further
//! restricts `Active` are out of scope until later milestones.
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

/// Connection-scoped timeouts (spec 4.7), applied by callers via
/// `tokio::time::timeout` around the relevant phase -- this module stays a
/// pure state machine with no clock of its own (see the module docs).
pub mod defaults {
    use std::time::Duration;

    /// `Handshaking` phase (ClientHello/ServerHello exchange).
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    /// `Authenticating` phase, reset on each attempt (spec 4.7). This PoC
    /// does not implement the `AuthChallengeRenew` retry loop (M2's
    /// documented out-of-scope item), so in practice this bounds the
    /// single AuthPubkey/AuthResult round trip.
    pub const AUTH_TIMEOUT: Duration = Duration::from_secs(60);
    /// `Authenticated` phase: time allowed to reach `Active` (spec 4.1,
    /// DR-024's first-Channel-Live trigger).
    pub const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(15);
    /// `Active` phase: time since the last received message before
    /// `Active -> Suspended` (spec 4.1, 3x `KEEPALIVE_INTERVAL`).
    pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
    /// Period `KeepAlive` is sent on, both directions (spec 2.9).
    pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
    /// `Suspended` phase: time allowed for a reconnection before the
    /// session is permanently `Closed` (spec 4.1, 4.6).
    pub const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(300);
    /// `Closing` phase: grace period before the transport connection is
    /// actually torn down (spec 4.1, 4.7).
    pub const CLOSING_GRACE_PERIOD: Duration = Duration::from_secs(2);
}

/// Connection SM states (spec 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Handshaking,
    Authenticating,
    Authenticated,
    Active,
    /// The transport connection is gone (IDLE_TIMEOUT or an actual
    /// disconnect); a new connection may resume this session within
    /// `RECONNECT_GRACE_PERIOD` via the Reconnection SM (spec 4.6).
    Suspended,
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

    /// Spec 4.1 Active row: "すべて許可(ClientHello/ServerHello/
    /// AuthChallengeRenewを除く)". `AuthChallengeRenew` isn't implemented
    /// (M2 scope), so only the Hello resend case applies here.
    fn allowed_in_active(type_id: u16) -> bool {
        !matches!(type_id, type_id::CLIENT_HELLO | type_id::SERVER_HELLO)
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
            ConnectionState::Active => Self::allowed_in_active(type_id),
            // Spec 4.1: "接続が存在しないため本状態自体にはメッセージ往来が
            // ない" -- Suspended's only way out is the Reconnection SM
            // (spec 4.6) on a *new* connection, not a message on this one.
            ConnectionState::Suspended => false,
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

    /// `Authenticated -> Active`, on the first VideoStream Channel
    /// reaching `Live` (spec 4.1, DR-024). Each side (client/server)
    /// calls this from its own observation of its own
    /// [`crate::channel_sm::ChannelSm`] reaching `Live`.
    pub fn on_channel_live(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            ConnectionState::Authenticated => {
                self.state = ConnectionState::Active;
                Ok(())
            }
            _ => Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            }),
        }
    }

    /// `Active -> Suspended`: the transport connection is gone, whether
    /// from `IDLE_TIMEOUT` or an actual disconnect (spec 4.1). Does not
    /// itself start the `RECONNECT_GRACE_PERIOD` clock or touch any
    /// session store -- that's the caller's job (spec 4.6); this only
    /// tracks the state transition itself.
    pub fn suspend(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            ConnectionState::Active => {
                self.state = ConnectionState::Suspended;
                Ok(())
            }
            _ => Err(ProtocolViolation {
                reason: ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            }),
        }
    }

    /// `Suspended -> Active`, once a reconnection on a new connection is
    /// accepted (spec 4.6: "ReconnectAccepted -> \[Connection SM:
    /// Active\]"). Skips back through Authenticating/Authenticated
    /// deliberately: `AuthPubkey` is not re-verified on reconnect (the
    /// `reconnect_token` alone is spec 4.6's proof of continuity), so
    /// there is no second `Authenticated` moment to pass through.
    pub fn resume(&mut self) -> Result<(), ProtocolViolation> {
        match self.state {
            ConnectionState::Suspended => {
                self.state = ConnectionState::Active;
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
    fn channel_live_transitions_authenticated_to_active() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.complete_authentication().unwrap();
        assert_eq!(sm.on_channel_live(), Ok(()));
        assert_eq!(sm.state(), ConnectionState::Active);
    }

    #[test]
    fn channel_live_before_authenticated_is_rejected() {
        let mut sm = ConnectionSm::new();
        assert!(sm.on_channel_live().is_err());
        assert_eq!(sm.state(), ConnectionState::Handshaking);
    }

    #[test]
    fn active_rejects_hello_resend_but_allows_other_messages() {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.complete_authentication().unwrap();
        sm.on_channel_live().unwrap();
        assert!(sm.check_message(type_id::CLIENT_HELLO).is_err());
        assert!(sm.check_message(type_id::SERVER_HELLO).is_err());
        assert_eq!(sm.check_message(type_id::TRANSPORT_FEEDBACK), Ok(()));
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

    fn active_sm() -> ConnectionSm {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.complete_authentication().unwrap();
        sm.on_channel_live().unwrap();
        sm
    }

    #[test]
    fn suspend_transitions_active_to_suspended() {
        let mut sm = active_sm();
        assert_eq!(sm.suspend(), Ok(()));
        assert_eq!(sm.state(), ConnectionState::Suspended);
    }

    #[test]
    fn suspend_outside_active_is_rejected() {
        let mut sm = ConnectionSm::new();
        assert!(sm.suspend().is_err());
        assert_eq!(sm.state(), ConnectionState::Handshaking);
    }

    #[test]
    fn suspended_rejects_all_messages() {
        let mut sm = active_sm();
        sm.suspend().unwrap();
        assert!(sm.check_message(type_id::TRANSPORT_FEEDBACK).is_err());
        assert!(sm.check_message(type_id::KEEP_ALIVE).is_err());
    }

    #[test]
    fn resume_transitions_suspended_to_active() {
        let mut sm = active_sm();
        sm.suspend().unwrap();
        assert_eq!(sm.resume(), Ok(()));
        assert_eq!(sm.state(), ConnectionState::Active);
    }

    #[test]
    fn resume_outside_suspended_is_rejected() {
        let mut sm = active_sm();
        assert!(sm.resume().is_err());
        assert_eq!(sm.state(), ConnectionState::Active);
    }

    #[test]
    fn resumed_connection_behaves_like_a_normal_active_connection() {
        let mut sm = active_sm();
        sm.suspend().unwrap();
        sm.resume().unwrap();
        assert_eq!(sm.check_message(type_id::TRANSPORT_FEEDBACK), Ok(()));
        assert!(sm.check_message(type_id::CLIENT_HELLO).is_err());
    }
}
