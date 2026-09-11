//! Client-side persistence of `session_id`/`reconnect_token`/`user_id`
//! across process runs, for `sardp-client`'s `--session-file` flag: a
//! fresh process that finds this file reconnects (`SessionReauthenticate`,
//! spec 4.6) instead of doing a fresh `ClientHello` handshake. This is the
//! only realistic way to demonstrate a reconnection round-trip between two
//! real binary invocations (a real always-up client would auto-reconnect
//! in-process instead, out of scope for this PoC).
//!
//! **Demo/test use only** (KNOWN_ISSUES.md #4): `reconnect_token` is a
//! bearer credential -- possession alone resumes the original session --
//! and this writes it in plain text with no permission control. Anyone who
//! can read this file can hijack the session it names.

use std::path::Path;

/// Session state persisted across process runs. Plain hex/text, not any
/// format worth a dependency for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSession {
    pub session_id: [u8; 16],
    pub reconnect_token: [u8; 32],
    pub user_id: String,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn parse_saved_session(contents: &str) -> Option<SavedSession> {
    let mut lines = contents.lines();
    let session_id: [u8; 16] = parse_hex_bytes(lines.next()?)?.try_into().ok()?;
    let reconnect_token: [u8; 32] = parse_hex_bytes(lines.next()?)?.try_into().ok()?;
    let user_id = lines.next()?.to_string();
    Some(SavedSession {
        session_id,
        reconnect_token,
        user_id,
    })
}

fn format_saved_session(session: &SavedSession) -> String {
    format!(
        "{}\n{}\n{}\n",
        hex_encode(&session.session_id),
        hex_encode(&session.reconnect_token),
        session.user_id
    )
}

/// Reads a previously-written `--session-file`. `None` on a missing file
/// or any parse failure -- the caller falls back to a fresh handshake.
pub fn read_saved_session(path: &Path) -> Option<SavedSession> {
    let contents = std::fs::read_to_string(path).ok()?;
    parse_saved_session(&contents)
}

/// Writes `session` to `path`. Logs (not panics) on failure: the demo flow
/// works fine without persistence, just without reconnect on the next run.
pub fn write_saved_session(path: &Path, session: &SavedSession) {
    if let Err(e) = std::fs::write(path, format_saved_session(session)) {
        eprintln!("warning: failed to write --session-file {path:?}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_format_and_parse() {
        let session = SavedSession {
            session_id: [0x11; 16],
            reconnect_token: [0x22; 32],
            user_id: "demo-user".to_string(),
        };
        let formatted = format_saved_session(&session);
        assert_eq!(parse_saved_session(&formatted), Some(session));
    }

    #[test]
    fn reads_and_writes_a_real_file() {
        let dir =
            std::env::temp_dir().join(format!("sardp-session-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session");
        let session = SavedSession {
            session_id: [0xAB; 16],
            reconnect_token: [0xCD; 32],
            user_id: "roundtrip-user".to_string(),
        };
        write_saved_session(&path, &session);
        assert_eq!(read_saved_session(&path), Some(session));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_reads_as_none() {
        let path = std::env::temp_dir().join("sardp-session-file-does-not-exist");
        assert_eq!(read_saved_session(&path), None);
    }

    #[test]
    fn rejects_odd_length_hex() {
        assert_eq!(parse_saved_session("abc\ndeadbeef\nuser\n"), None);
    }

    #[test]
    fn rejects_wrong_length_session_id() {
        // Only 2 bytes where session_id needs 16.
        assert_eq!(parse_saved_session("aabb\ndeadbeef\nuser\n"), None);
    }

    #[test]
    fn rejects_missing_lines() {
        assert_eq!(parse_saved_session(&hex_encode(&[1u8; 16])), None);
    }
}
