//! StreamPrologue: the fixed structure (not Envelope-wrapped) sent as the
//! first bytes of every newly opened stream (spec 2.2).
//!
//! ```text
//! StreamPrologue {
//!     magic      : bytes(4)   // "SARD" = 0x53 0x41 0x52 0x44
//!     kind       : u8
//!     version    : u8
//!     context_id : varint
//! }
//! ```

use crate::stream_kind::StreamKind;
use crate::varint;

/// `StreamPrologue.magic`, spec 2.2.
pub const MAGIC: [u8; 4] = *b"SARD";

/// A parsed StreamPrologue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPrologue {
    pub kind: StreamKind,
    /// Message-system version for `kind`. Spec 2.2 states this is
    /// "currently always 1" but, unlike a magic mismatch or unknown kind,
    /// does not itself mandate aborting the stream on other values — so
    /// this parser surfaces it unvalidated for the caller to act on.
    pub version: u8,
    pub context_id: u64,
}

/// StreamPrologue parsing failed in a way that is not just "need more
/// bytes". Per spec 2.2, both variants MUST cause the implementation to
/// abort the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrologueError {
    /// The first 4 bytes did not equal `"SARD"`.
    MagicMismatch,
    /// The `kind` byte is not one of the values in the spec 2.2.1 table.
    UnknownKind(u8),
}

/// Encodes a StreamPrologue (`magic` + `kind` + `version` + `context_id`)
/// onto the end of `out`.
pub fn encode(kind: StreamKind, version: u8, context_id: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&MAGIC);
    out.push(kind.to_byte());
    out.push(version);
    varint::encode(context_id, out);
}

/// Attempts to decode one StreamPrologue from the start of `buf`.
///
/// Returns:
/// - `Ok(Some((prologue, consumed)))` on a complete, valid StreamPrologue.
/// - `Ok(None)` if `buf` does not yet contain a complete StreamPrologue.
/// - `Err(_)` on a magic mismatch or unknown `kind`; per spec 2.2 the
///   caller MUST abort the stream in either case. `kind` is checked (and,
///   if invalid, rejected) as soon as its byte is available, without
///   waiting for `context_id` to arrive in full.
///
/// Never panics on attacker-controlled input.
pub fn parse(buf: &[u8]) -> Result<Option<(StreamPrologue, usize)>, PrologueError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    if buf[0..4] != MAGIC {
        return Err(PrologueError::MagicMismatch);
    }
    if buf.len() < 6 {
        return Ok(None);
    }
    let kind_byte = buf[4];
    let version = buf[5];
    let kind = StreamKind::from_byte(kind_byte).ok_or(PrologueError::UnknownKind(kind_byte))?;

    let (context_id, context_id_len) = match varint::decode(&buf[6..]) {
        Ok(v) => v,
        Err(varint::VarintError::Incomplete) => return Ok(None),
    };

    let consumed = 6 + context_id_len;
    Ok(Some((
        StreamPrologue {
            kind,
            version,
            context_id,
        },
        consumed,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let mut out = Vec::new();
        encode(StreamKind::Video, 1, 300, &mut out);
        let (prologue, consumed) = parse(&out).unwrap().unwrap();
        assert_eq!(consumed, out.len());
        assert_eq!(prologue.kind, StreamKind::Video);
        assert_eq!(prologue.version, 1);
        assert_eq!(prologue.context_id, 300);
    }

    fn build(kind: u8, version: u8, context_id_byte: u8) -> Vec<u8> {
        let mut buf = MAGIC.to_vec();
        buf.push(kind);
        buf.push(version);
        buf.push(context_id_byte); // 1-byte varint form (top 2 bits 00)
        buf
    }

    #[test]
    fn parses_control_prologue() {
        let buf = build(0x01, 1, 0);
        let (prologue, consumed) = parse(&buf).unwrap().unwrap();
        assert_eq!(consumed, 7);
        assert_eq!(prologue.kind, StreamKind::Control);
        assert_eq!(prologue.version, 1);
        assert_eq!(prologue.context_id, 0);
    }

    #[test]
    fn parses_video_prologue_with_monitor_context_id() {
        let buf = build(0x03, 1, 5);
        let (prologue, _) = parse(&buf).unwrap().unwrap();
        assert_eq!(prologue.kind, StreamKind::Video);
        assert_eq!(prologue.context_id, 5);
    }

    #[test]
    fn parses_all_known_kinds() {
        for (byte, expected) in [
            (0x01, StreamKind::Control),
            (0x02, StreamKind::Input),
            (0x03, StreamKind::Video),
            (0x04, StreamKind::Feedback),
            (0x05, StreamKind::Clipboard),
            (0x06, StreamKind::File),
            (0x07, StreamKind::AudioPlayback),
            (0x08, StreamKind::AudioCapture),
        ] {
            let buf = build(byte, 1, 0);
            let (prologue, _) = parse(&buf).unwrap().unwrap();
            assert_eq!(prologue.kind, expected);
        }
    }

    #[test]
    fn magic_mismatch_is_rejected() {
        let mut buf = build(0x01, 1, 0);
        buf[0] = b'X';
        assert_eq!(parse(&buf), Err(PrologueError::MagicMismatch));
    }

    #[test]
    fn completely_wrong_magic_is_rejected() {
        let buf = b"XXXX\x01\x01\x00".to_vec();
        assert_eq!(parse(&buf), Err(PrologueError::MagicMismatch));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let buf = build(0x09, 1, 0);
        assert_eq!(parse(&buf), Err(PrologueError::UnknownKind(0x09)));
    }

    #[test]
    fn zero_kind_is_rejected() {
        let buf = build(0x00, 1, 0);
        assert_eq!(parse(&buf), Err(PrologueError::UnknownKind(0x00)));
    }

    #[test]
    fn unknown_kind_is_rejected_even_if_context_id_not_yet_buffered() {
        // magic + kind + version present, but the context_id varint byte
        // has not arrived yet. Kind is still invalid immediately.
        let mut buf = MAGIC.to_vec();
        buf.push(0xFF);
        buf.push(1);
        assert_eq!(parse(&buf), Err(PrologueError::UnknownKind(0xFF)));
    }

    #[test]
    fn version_is_surfaced_unvalidated() {
        // Spec text only mandates aborting on magic mismatch / unknown
        // kind; an unexpected version value is passed through for the
        // caller to decide, not rejected by this parser.
        let buf = build(0x01, 99, 0);
        let (prologue, _) = parse(&buf).unwrap().unwrap();
        assert_eq!(prologue.version, 99);
    }

    #[test]
    fn empty_buffer_is_incomplete() {
        assert_eq!(parse(&[]), Ok(None));
    }

    #[test]
    fn partial_magic_is_incomplete() {
        assert_eq!(parse(b"SAR"), Ok(None));
    }

    #[test]
    fn magic_only_is_incomplete() {
        assert_eq!(parse(b"SARD"), Ok(None));
    }

    #[test]
    fn magic_plus_kind_missing_version_is_incomplete() {
        let mut buf = MAGIC.to_vec();
        buf.push(0x01);
        assert_eq!(parse(&buf), Ok(None));
    }

    #[test]
    fn missing_context_id_byte_is_incomplete() {
        let mut buf = MAGIC.to_vec();
        buf.push(0x01);
        buf.push(1);
        assert_eq!(parse(&buf), Ok(None));
    }

    #[test]
    fn truncated_multibyte_context_id_is_incomplete() {
        let mut buf = MAGIC.to_vec();
        buf.push(0x01); // control
        buf.push(1); // version
        buf.push(0x7F); // 2-byte varint form prefix, second byte missing
        assert_eq!(parse(&buf), Ok(None));
    }

    #[test]
    fn multi_byte_context_id_decodes_correctly() {
        let mut buf = MAGIC.to_vec();
        buf.push(0x03); // video
        buf.push(1);
        // 2-byte varint form encoding 300: 0b01_000001_00101100 = 0x41 0x2C
        buf.push(0x41);
        buf.push(0x2C);
        let (prologue, consumed) = parse(&buf).unwrap().unwrap();
        assert_eq!(prologue.context_id, 300);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn does_not_consume_trailing_bytes() {
        let mut buf = build(0x01, 1, 0);
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let (_, consumed) = parse(&buf).unwrap().unwrap();
        assert_eq!(consumed, 7);
    }
}
