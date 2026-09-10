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
        let expiry_ts = clock::now_us() + ttl.as_micros() as u64;
        let record = FileHandleRecord {
            session_id,
            user_id,
            direction,
            resolved_size,
            expiry_ts,
        };
        let mut handles = self.handles.lock().unwrap();
        loop {
            let mut candidate_bytes = [0u8; 8];
            rand::rng().fill_bytes(&mut candidate_bytes);
            let candidate = u64::from_le_bytes(candidate_bytes) & crate::varint::MAX;
            if let Entry::Vacant(entry) = handles.entry(candidate) {
                entry.insert(record);
                return (candidate, expiry_ts);
            }
        }
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
}
