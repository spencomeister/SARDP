//! Server-side registry of issued `file_handle`s (spec 2.6: "不透明ハンドル。
//! セッション・ユーザー・方向・有効期限に束縛"). DR-037 (this
//! implementation's own decision, following on from DR-036): the server
//! MUST verify that binding -- the `file` stream's caller is the same
//! `session_id`/`user_id`, requesting the same `direction`, before the
//! `expiry_ts` deadline -- before treating a presented `file_handle` as
//! authorization to read or write that transfer's data. Without this, any
//! connection that merely learned another session's `file_handle` (e.g. by
//! observing it, or simply guessing a small handle space) could hijack that
//! transfer; see `sardp-server`'s `handle_file_transfer` for where this is
//! enforced against the `file` stream's actual opener.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Mutex;
use std::time::Duration;

use rand::Rng;

use crate::clock;
use crate::messages::FileTransferDirection;

/// Everything a `file_handle` is bound to at issuance (spec 2.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHandleRecord {
    pub session_id: [u8; 16],
    pub user_id: String,
    pub direction: FileTransferDirection,
    pub resolved_size: u64,
    /// Monotonic-clock deadline (`crate::clock::now_us()` basis) after
    /// which this handle is no longer valid (spec 2.6 `expiry_ts`).
    pub expiry_ts: u64,
}

/// Why [`FileHandleStore::validate`] refused a `file_handle` (DR-037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileHandleError {
    Unknown,
    Expired,
    SessionMismatch,
    UserMismatch,
    DirectionMismatch,
}

/// Issued `file_handle`s, keyed by the handle itself.
#[derive(Default)]
pub struct FileHandleStore {
    handles: Mutex<HashMap<u64, FileHandleRecord>>,
}

impl FileHandleStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a fresh, CSPRNG-generated `file_handle` bound to the given
    /// session/user/direction/size, valid until `crate::clock::now_us() +
    /// ttl`. Retries on the astronomically unlikely event of a collision
    /// with a still-live handle, matching [`crate::session_store`]'s own
    /// `Entry`-based atomicity.
    ///
    /// Masked to `crate::varint::MAX` (2^62-1, not the full `u64` range):
    /// `file_handle` doubles as the `file` stream's `StreamPrologue.context_id`
    /// (see `crate::messages::FileTransferAccept`'s doc comment), which is
    /// a varint that can't carry the top two bits of an arbitrary `u64`.
    pub fn issue(
        &self,
        session_id: [u8; 16],
        user_id: String,
        direction: FileTransferDirection,
        resolved_size: u64,
        ttl: Duration,
    ) -> (u64, u64) {
        self.try_issue(
            session_id,
            user_id,
            direction,
            resolved_size,
            ttl,
            usize::MAX,
        )
        .expect("usize::MAX capacity is never reached")
    }

    /// Like [`Self::issue`], but refuses (returns `None`, touching nothing)
    /// if `self` already holds `max_concurrent` outstanding handles --
    /// issued but not yet [`Self::remove`]d, whether or not their `file`
    /// stream has even been opened yet. Without this, a client that keeps
    /// sending `FileTransferRequest` without ever opening the resulting
    /// `file` stream (or simply requesting far more transfers than it
    /// finishes) can grow this store without bound (KNOWN_ISSUES.md #3).
    pub fn try_issue(
        &self,
        session_id: [u8; 16],
        user_id: String,
        direction: FileTransferDirection,
        resolved_size: u64,
        ttl: Duration,
        max_concurrent: usize,
    ) -> Option<(u64, u64)> {
        let expiry_ts = clock::now_us() + ttl.as_micros() as u64;
        let record = FileHandleRecord {
            session_id,
            user_id,
            direction,
            resolved_size,
            expiry_ts,
        };
        let mut handles = self.handles.lock().unwrap();
        if handles.len() >= max_concurrent {
            return None;
        }
        loop {
            let mut candidate_bytes = [0u8; 8];
            rand::rng().fill_bytes(&mut candidate_bytes);
            let candidate = u64::from_le_bytes(candidate_bytes) & crate::varint::MAX;
            if let Entry::Vacant(entry) = handles.entry(candidate) {
                entry.insert(record);
                return Some((candidate, expiry_ts));
            }
        }
    }

    /// Removes every handle whose `expiry_ts` has already passed,
    /// regardless of whether its `file` stream was ever opened. Without an
    /// active sweep like this, a handle nobody ever presented a `file`
    /// stream for sits in the map forever: [`Self::validate`] already
    /// treats it as `Expired`, but nothing previously reclaimed the entry
    /// itself (KNOWN_ISSUES.md #3). Returns how many were removed.
    pub fn sweep_expired(&self) -> usize {
        let now = clock::now_us();
        let mut handles = self.handles.lock().unwrap();
        let before = handles.len();
        handles.retain(|_, record| record.expiry_ts >= now);
        before - handles.len()
    }

    /// Spawns a task that calls [`Self::sweep_expired`] every `interval`.
    /// Holds only a `Weak` reference to `self`, so the task ends on its
    /// own once every other `Arc<FileHandleStore>` (e.g. `ServerState`'s)
    /// is dropped, rather than keeping the store alive by itself.
    pub fn spawn_reaper(
        self: &std::sync::Arc<Self>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let weak = std::sync::Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                let Some(store) = weak.upgrade() else {
                    return;
                };
                store.sweep_expired();
            }
        })
    }

    /// DR-037: verifies `file_handle` was issued to exactly this
    /// session/user/direction and hasn't expired. Does *not* consume the
    /// handle on success -- unlike a `reconnect_token`, spec 2.6 doesn't
    /// call for single-use handles (a transfer may need to reopen the
    /// `file` stream, e.g. after a reset, any number of times before
    /// `expiry_ts`); see [`Self::remove`] for the actual end-of-life.
    pub fn validate(
        &self,
        file_handle: u64,
        session_id: [u8; 16],
        user_id: &str,
        direction: FileTransferDirection,
    ) -> Result<FileHandleRecord, FileHandleError> {
        let handles = self.handles.lock().unwrap();
        let record = handles.get(&file_handle).ok_or(FileHandleError::Unknown)?;
        if record.session_id != session_id {
            return Err(FileHandleError::SessionMismatch);
        }
        if record.user_id != user_id {
            return Err(FileHandleError::UserMismatch);
        }
        if record.direction != direction {
            return Err(FileHandleError::DirectionMismatch);
        }
        if clock::now_us() > record.expiry_ts {
            return Err(FileHandleError::Expired);
        }
        Ok(record.clone())
    }

    /// Removes a handle once its transfer has ended (spec 2.6's temp-state
    /// table: `file_handle`'s scope ends when `FileTransferComplete`/
    /// `FileTransferError` is sent). A no-op if already gone.
    pub fn remove(&self, file_handle: u64) {
        self.handles.lock().unwrap().remove(&file_handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn issuing_and_validating_with_matching_details_succeeds() {
        let store = FileHandleStore::new();
        let session_id = [1u8; 16];
        let (handle, expiry_ts) = store.issue(
            session_id,
            "alice".into(),
            FileTransferDirection::Upload,
            1024,
            TTL,
        );

        let record = store
            .validate(handle, session_id, "alice", FileTransferDirection::Upload)
            .expect("matching session/user/direction validates");
        assert_eq!(record.resolved_size, 1024);
        assert_eq!(record.expiry_ts, expiry_ts);
    }

    #[test]
    fn unknown_handle_is_rejected() {
        let store = FileHandleStore::new();
        assert_eq!(
            store.validate(0xDEAD_BEEF, [0; 16], "alice", FileTransferDirection::Upload),
            Err(FileHandleError::Unknown)
        );
    }

    #[test]
    fn a_different_session_id_is_rejected() {
        let store = FileHandleStore::new();
        let (handle, _) = store.issue(
            [1u8; 16],
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            TTL,
        );
        assert_eq!(
            store.validate(handle, [2u8; 16], "alice", FileTransferDirection::Upload),
            Err(FileHandleError::SessionMismatch)
        );
    }

    #[test]
    fn a_different_user_id_is_rejected() {
        let store = FileHandleStore::new();
        let session_id = [1u8; 16];
        let (handle, _) = store.issue(
            session_id,
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            TTL,
        );
        assert_eq!(
            store.validate(handle, session_id, "mallory", FileTransferDirection::Upload),
            Err(FileHandleError::UserMismatch)
        );
    }

    #[test]
    fn a_different_direction_is_rejected() {
        let store = FileHandleStore::new();
        let session_id = [1u8; 16];
        let (handle, _) = store.issue(
            session_id,
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            TTL,
        );
        assert_eq!(
            store.validate(handle, session_id, "alice", FileTransferDirection::Download),
            Err(FileHandleError::DirectionMismatch)
        );
    }

    #[test]
    fn an_expired_handle_is_rejected() {
        let store = FileHandleStore::new();
        let session_id = [1u8; 16];
        let (handle, _) = store.issue(
            session_id,
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            Duration::from_micros(0),
        );
        assert_eq!(
            store.validate(handle, session_id, "alice", FileTransferDirection::Upload),
            Err(FileHandleError::Expired)
        );
    }

    #[test]
    fn removed_handle_is_unknown_afterward() {
        let store = FileHandleStore::new();
        let session_id = [1u8; 16];
        let (handle, _) = store.issue(
            session_id,
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            TTL,
        );
        store.remove(handle);
        assert_eq!(
            store.validate(handle, session_id, "alice", FileTransferDirection::Upload),
            Err(FileHandleError::Unknown)
        );
    }

    #[test]
    fn try_issue_refuses_once_at_capacity() {
        let store = FileHandleStore::new();
        let issue_one = |store: &FileHandleStore| {
            store.try_issue(
                [1u8; 16],
                "alice".into(),
                FileTransferDirection::Upload,
                0,
                TTL,
                2,
            )
        };
        assert!(issue_one(&store).is_some());
        assert!(issue_one(&store).is_some());
        assert_eq!(
            issue_one(&store),
            None,
            "a third handle must be refused once 2 are already outstanding"
        );
    }

    #[test]
    fn try_issue_has_room_again_after_a_handle_is_removed() {
        let store = FileHandleStore::new();
        let (handle, _) = store
            .try_issue(
                [1u8; 16],
                "alice".into(),
                FileTransferDirection::Upload,
                0,
                TTL,
                1,
            )
            .unwrap();
        assert!(
            store
                .try_issue(
                    [1u8; 16],
                    "alice".into(),
                    FileTransferDirection::Upload,
                    0,
                    TTL,
                    1,
                )
                .is_none()
        );
        store.remove(handle);
        assert!(
            store
                .try_issue(
                    [1u8; 16],
                    "alice".into(),
                    FileTransferDirection::Upload,
                    0,
                    TTL,
                    1,
                )
                .is_some()
        );
    }

    #[test]
    fn issue_is_unaffected_by_capacity() {
        // issue() itself (used by callers that don't care about the cap --
        // e.g. tests) must keep working exactly as before.
        let store = FileHandleStore::new();
        for _ in 0..5 {
            store.issue(
                [1u8; 16],
                "alice".into(),
                FileTransferDirection::Upload,
                0,
                TTL,
            );
        }
    }

    #[test]
    fn sweep_expired_removes_only_past_deadline_handles() {
        let store = FileHandleStore::new();
        let (expired, _) = store.issue(
            [1u8; 16],
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            Duration::from_micros(0),
        );
        let (live, _) = store.issue(
            [1u8; 16],
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            TTL,
        );
        assert_eq!(store.sweep_expired(), 1);
        assert_eq!(
            store.validate(expired, [1u8; 16], "alice", FileTransferDirection::Upload),
            Err(FileHandleError::Unknown)
        );
        assert!(
            store
                .validate(live, [1u8; 16], "alice", FileTransferDirection::Upload)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn spawn_reaper_periodically_removes_expired_handles() {
        let store = std::sync::Arc::new(FileHandleStore::new());
        let (handle, _) = store.issue(
            [1u8; 16],
            "alice".into(),
            FileTransferDirection::Upload,
            0,
            Duration::from_millis(1),
        );
        let reaper = store.spawn_reaper(Duration::from_millis(20));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            store.validate(handle, [1u8; 16], "alice", FileTransferDirection::Upload),
            Err(FileHandleError::Unknown),
            "the reaper should have swept the expired handle by now"
        );
        reaper.abort();
    }

    #[tokio::test]
    async fn spawn_reaper_stops_once_the_store_is_dropped() {
        let store = std::sync::Arc::new(FileHandleStore::new());
        let reaper = store.spawn_reaper(Duration::from_millis(10));
        drop(store);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            reaper.is_finished(),
            "the reaper must end once the only other Arc is dropped"
        );
    }
}
