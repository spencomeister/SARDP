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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn expire(&self, session_id: [u8; 16]) {
        self.sessions.lock().unwrap().remove(&session_id);
    }

    /// Whether a session is currently suspended and awaiting reconnection.
    pub fn contains(&self, session_id: [u8; 16]) -> bool {
        self.sessions.lock().unwrap().contains_key(&session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_sm::ConnectionState;

    fn suspended_sm() -> ConnectionSm {
        let mut sm = ConnectionSm::new();
        sm.complete_handshake().unwrap();
        sm.complete_authentication().unwrap();
        sm.on_channel_live().unwrap();
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
                connection_sm: suspended_sm(),
                granted_permissions: 0,
                last_generation: 0,
            },
        );
        store.try_reconnect(session_id, [2; 32]).unwrap();
        store.expire(session_id); // must not panic or affect anything else
        assert!(!store.contains(session_id));
    }
}
