//! Server-side store of `Suspended` sessions awaiting reconnection (spec
//! 4.6). Keyed by `session_id`; a `reconnect_token` is validated and
//! atomically consumed (removed from the store) on a successful match,
//! preventing a stolen or replayed token from being used twice -- spec
//! 2.3's "アトミックな消費" requirement (there written with a clustered
//! server deployment in mind; a single in-process `Mutex` satisfies the
//! same requirement for this PoC's single-process server).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Mutex;

use subtle::ConstantTimeEq;

use crate::connection_sm::ConnectionSm;

/// Everything a `Suspended` session needs to resume on a new connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendedSession {
    pub reconnect_token: [u8; 32],
    /// Already `Suspended` (spec 4.1) at the moment it's stored here;
    /// [`crate::reconnection::server_complete_reconnect`] resumes it to
    /// `Active` on a successful match.
    pub connection_sm: ConnectionSm,
    pub granted_permissions: u32,
    /// The video Channel's generation at the moment of suspension. A
    /// resumed session's new Instance opens at this value + 1 (spec 4.6,
    /// DR-026: generation continues monotonically across a reconnect, it
    /// never resets).
    pub last_generation: u64,
    /// `AuthPubkey.user_id` (spec 2.3), carried across suspend/resume so a
    /// later re-suspension of a *resumed* session still has it (needed for
    /// the `file_handle` ownership binding, spec 2.6/DR-037; see
    /// `crate::handshake::HandshakeOutcome::user_id`).
    pub user_id: String,
}

/// Why [`SessionStore::try_reconnect`] failed. Spec 4.6's ReasonCode table
/// does not distinguish "no such session" from "wrong token" -- both are
/// `AUTH.5 RECONNECT_TOKEN_INVALID` on the wire -- but this type keeps them
/// separate internally since they're worth telling apart in logs/tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectError {
    NoSuchSession,
    TokenMismatch,
}

/// Suspended sessions awaiting reconnection, keyed by `session_id`.
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<[u8; 16], SuspendedSession>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a `Suspended` session, replacing any existing entry for
    /// the same `session_id` (there should never be one: a session_id is
    /// only ever suspended once before being either resumed or expired).
    pub fn suspend(&self, session_id: [u8; 16], session: SuspendedSession) {
        self.sessions.lock().unwrap().insert(session_id, session);
    }

    /// Atomically validates and consumes a reconnect attempt: on a match,
    /// the session is removed (its token is now spent) and returned;
    /// on any failure, the store is left exactly as it was, so a client
    /// that made a typo (or an attacker guessing) doesn't get to
    /// invalidate the legitimate session's chance to reconnect.
    ///
    /// The token comparison itself is constant-time (`subtle`'s
    /// `ct_eq`, not `==`): a `reconnect_token` is a bearer credential --
    /// spec 2.3/4.6's whole reason for making it single-use and atomically
    /// consumed is to resist theft/replay, which a byte-by-byte
    /// short-circuiting `==` would partially undermine by leaking timing
    /// information about how many leading bytes an attacker's guess got
    /// right.
    pub fn try_reconnect(
        &self,
        session_id: [u8; 16],
        token: [u8; 32],
    ) -> Result<SuspendedSession, ReconnectError> {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.entry(session_id) {
            Entry::Occupied(entry) => {
                if bool::from(entry.get().reconnect_token.ct_eq(&token)) {
                    Ok(entry.remove())
                } else {
                    Err(ReconnectError::TokenMismatch)
                }
            }
            Entry::Vacant(_) => Err(ReconnectError::NoSuchSession),
        }
    }

    /// Removes a session unconditionally (spec 4.6:
    /// `RECONNECT_GRACE_PERIOD` elapsed with no reconnection). A no-op if
    /// it's already gone (e.g. a reconnect already consumed it, or it was
    /// never there).
    ///
    /// Callers scheduling this ahead of time for a *specific* suspend
    /// episode (e.g. a timer armed when `suspend()` is called) almost
    /// always want [`Self::expire_if_token_matches`] instead: this method
    /// removes whatever currently sits under `session_id`, which is wrong
    /// if the session was reconnected and suspended again in the meantime
    /// (a fresh `reconnect_token`, and so a fresh `RECONNECT_GRACE_PERIOD`)
    /// before the original timer fired.
    pub fn expire(&self, session_id: [u8; 16]) {
        self.sessions.lock().unwrap().remove(&session_id);
    }

    /// Like [`Self::expire`], but only removes the entry if it's still the
    /// same suspend episode identified by `reconnect_token` -- a fresh
    /// token is issued every time a session is suspended (both the
    /// original suspend and any later reconnect), so this lets a timer
    /// armed for one specific suspend episode avoid clobbering a *later*
    /// one that reused the same `session_id` after an intervening
    /// reconnect. A no-op if the entry is already gone or belongs to a
    /// different episode (different token).
    ///
    /// Constant-time token comparison for the same reason as
    /// [`Self::try_reconnect`]: a `reconnect_token` is a bearer credential.
    pub fn expire_if_token_matches(&self, session_id: [u8; 16], reconnect_token: [u8; 32]) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Entry::Occupied(entry) = sessions.entry(session_id)
            && bool::from(entry.get().reconnect_token.ct_eq(&reconnect_token))
        {
            entry.remove();
        }
    }

    /// Whether a session is currently suspended and awaiting reconnection.
    pub fn contains(&self, session_id: [u8; 16]) -> bool {
        self.sessions.lock().unwrap().contains_key(&session_id)
    }

    /// Transitions `connection_sm` `Active -> Suspended` (spec 4.1) and
    /// registers it in `self` (`self` must be `Arc`-held so the spawned
    /// expiry task below can outlive the caller), so a new connection
    /// presenting `reconnect_token` can resume it within `grace_period`
    /// (spec 4.6: `RECONNECT_GRACE_PERIOD`). Spawns a task that expires
    /// the entry via [`Self::expire_if_token_matches`] if nobody
    /// reconnects in time -- not [`Self::expire`], since a reconnect
    /// followed by a second suspension before this timer fires would
    /// otherwise clobber that *later* suspend episode instead of the one
    /// this timer was armed for.
    ///
    /// Does nothing to `self` if `connection_sm.suspend()` itself fails
    /// (already not `Active`).
    #[allow(clippy::too_many_arguments)]
    pub fn suspend_and_schedule_expiry(
        self: &std::sync::Arc<Self>,
        session_id: [u8; 16],
        user_id: String,
        mut connection_sm: ConnectionSm,
        reconnect_token: [u8; 32],
        granted_permissions: u32,
        last_generation: u64,
        grace_period: std::time::Duration,
    ) -> Result<(), crate::connection_sm::ProtocolViolation> {
        connection_sm.suspend()?;
        self.suspend(
            session_id,
            SuspendedSession {
                reconnect_token,
                connection_sm,
                granted_permissions,
                last_generation,
                user_id,
            },
        );

        let store = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace_period).await;
            store.expire_if_token_matches(session_id, reconnect_token);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_sm::ConnectionState;

    fn active_sm() -> ConnectionSm {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.complete_authentication().unwrap();
        sm.on_channel_live().unwrap();
        sm
    }

    fn suspended_sm() -> ConnectionSm {
        let mut sm = active_sm();
        sm.suspend().unwrap();
        sm
    }

    #[test]
    fn reconnect_with_the_right_token_succeeds_and_consumes_it() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0b111,
                last_generation: 3,
            },
        );
        assert!(store.contains(session_id));

        let resumed = store
            .try_reconnect(session_id, [2; 32])
            .expect("matching token succeeds");
        assert_eq!(resumed.last_generation, 3);
        assert_eq!(resumed.granted_permissions, 0b111);
        assert_eq!(resumed.connection_sm.state(), ConnectionState::Suspended);

        // Consumed: the same session can't be reconnected to twice.
        assert!(!store.contains(session_id));
    }

    #[test]
    fn reconnect_with_the_wrong_token_fails_and_leaves_the_session_intact() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );

        assert_eq!(
            store.try_reconnect(session_id, [0xFF; 32]),
            Err(ReconnectError::TokenMismatch)
        );
        // A wrong guess must not have consumed the real session.
        assert!(store.contains(session_id));
        assert!(store.try_reconnect(session_id, [2; 32]).is_ok());
    }

    #[test]
    fn reconnect_to_an_unknown_session_fails() {
        let store = SessionStore::new();
        assert_eq!(
            store.try_reconnect([9; 16], [0; 32]),
            Err(ReconnectError::NoSuchSession)
        );
    }

    #[test]
    fn a_reconnect_token_cannot_be_reused() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        store.try_reconnect(session_id, [2; 32]).unwrap();
        assert_eq!(
            store.try_reconnect(session_id, [2; 32]),
            Err(ReconnectError::NoSuchSession),
            "the token was already spent; the session is simply gone now"
        );
    }

    #[test]
    fn expire_removes_a_session_that_was_never_reconnected() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        store.expire(session_id);
        assert!(!store.contains(session_id));
        assert_eq!(
            store.try_reconnect(session_id, [2; 32]),
            Err(ReconnectError::NoSuchSession)
        );
    }

    #[test]
    fn expire_is_a_no_op_if_already_reconnected() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        store.try_reconnect(session_id, [2; 32]).unwrap();
        store.expire(session_id); // must not panic or affect anything else
        assert!(!store.contains(session_id));
    }

    #[test]
    fn expire_if_token_matches_removes_the_matching_episode() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        store.expire_if_token_matches(session_id, [2; 32]);
        assert!(!store.contains(session_id));
    }

    #[test]
    fn expire_if_token_matches_is_a_no_op_for_a_stale_token() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [2; 32],
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        // A timer armed for some other (e.g. long-expired) episode must not
        // touch the entry currently sitting under this session_id.
        store.expire_if_token_matches(session_id, [0xFF; 32]);
        assert!(store.contains(session_id));
    }

    #[tokio::test]
    async fn suspend_and_schedule_expiry_stores_a_reconnectable_session() {
        let store = std::sync::Arc::new(SessionStore::new());
        let session_id = [1; 16];
        store
            .suspend_and_schedule_expiry(
                session_id,
                "alice".into(),
                active_sm(),
                [2; 32],
                0b111,
                3,
                std::time::Duration::from_secs(300),
            )
            .expect("Active connection_sm suspends cleanly");
        assert!(store.contains(session_id));
        let resumed = store
            .try_reconnect(session_id, [2; 32])
            .expect("matching token succeeds");
        assert_eq!(resumed.last_generation, 3);
    }

    #[tokio::test]
    async fn suspend_and_schedule_expiry_propagates_an_invalid_transition() {
        let store = std::sync::Arc::new(SessionStore::new());
        // A fresh ConnectionSm is Handshaking, not Active -- suspend() must
        // fail, and the store must be left untouched.
        let err = store
            .suspend_and_schedule_expiry(
                [1; 16],
                "alice".into(),
                ConnectionSm::new(),
                [2; 32],
                0,
                0,
                std::time::Duration::from_secs(300),
            )
            .unwrap_err();
        assert_eq!(
            err.reason,
            crate::reason_code::ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
        );
        assert!(!store.contains([1; 16]));
    }

    #[tokio::test]
    async fn suspend_and_schedule_expiry_expires_the_session_after_the_grace_period() {
        let store = std::sync::Arc::new(SessionStore::new());
        let session_id = [1; 16];
        store
            .suspend_and_schedule_expiry(
                session_id,
                "alice".into(),
                active_sm(),
                [2; 32],
                0,
                0,
                std::time::Duration::from_millis(20),
            )
            .unwrap();
        assert!(store.contains(session_id));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!store.contains(session_id));
    }

    #[tokio::test]
    async fn a_reconnect_before_expiry_survives_the_stale_timer() {
        let store = std::sync::Arc::new(SessionStore::new());
        let session_id = [1; 16];
        store
            .suspend_and_schedule_expiry(
                session_id,
                "alice".into(),
                active_sm(),
                [2; 32],
                0,
                0,
                std::time::Duration::from_millis(20),
            )
            .unwrap();

        let mut resumed = store.try_reconnect(session_id, [2; 32]).unwrap();
        resumed.connection_sm.resume().unwrap();
        resumed.connection_sm.suspend().unwrap();
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: [9; 32],
                ..resumed
            },
        );

        // The first suspend episode's timer (armed above for token [2; 32])
        // fires during this sleep; it must not clobber the second episode
        // (token [9; 32]) suspended just now.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(store.contains(session_id));
    }

    /// The regression this method exists for: a timer armed when a session
    /// is first suspended (episode A, token A) must not remove a *later*
    /// suspension of the same session_id (episode B, token B) reached via
    /// an intervening reconnect, even though `expire_if_token_matches` for
    /// episode A fires *after* episode B was stored.
    #[test]
    fn a_stale_expiry_timer_does_not_clobber_a_later_suspend_episode() {
        let store = SessionStore::new();
        let session_id = [1; 16];
        let token_a = [0xAA; 32];
        let token_b = [0xBB; 32];

        // Episode A: original suspend.
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: token_a,
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        // Reconnected (token A consumed) and later suspended again under a
        // fresh token -- episode B, still the same session_id.
        store.try_reconnect(session_id, token_a).unwrap();
        store.suspend(
            session_id,
            SuspendedSession {
                reconnect_token: token_b,
                user_id: "alice".into(),
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 1,
            },
        );

        // Episode A's timer finally fires. It must not remove episode B.
        store.expire_if_token_matches(session_id, token_a);
        assert!(
            store.contains(session_id),
            "a stale expiry for episode A must not remove episode B"
        );

        // Episode B's own timer correctly removes it.
        store.expire_if_token_matches(session_id, token_b);
        assert!(!store.contains(session_id));
    }
}
