//! Envelope: the per-message frame repeated after `StreamPrologue` on every
//! stream (spec 2.1.1).
//!
//! ```text
//! Envelope {
//!     length   : varint
//!     type     : u16
//!     payload  : bytes
//! }
//! ```
//!
//! `length` counts `payload` only, not `type` (spec 2.1.1, DR-032).

use crate::varint;

/// Bit 15 of `Envelope.type`: 1 = unknown types on this bit MAY be ignored.
const IGNORABLE_FLAG: u16 = 0x8000;
/// Bits 14:0 of `Envelope.type`: the type id proper.
const TYPE_ID_MASK: u16 = 0x7FFF;
/// Upper bound (inclusive) of the `core` type id range (spec 2.1.1).
const CORE_TYPE_ID_MAX: u16 = 0x3FFF;

/// A parsed Envelope header plus a zero-copy view of its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// The raw 16-bit `type` field, flag bit included.
    pub type_raw: u16,
    pub payload: &'a [u8],
}

impl<'a> Envelope<'a> {
    /// `bit15`: unknown types with this bit set MUST be skipped rather than
    /// treated as a protocol violation (spec 2.1.1).
    pub fn is_ignorable(&self) -> bool {
        self.type_raw & IGNORABLE_FLAG != 0
    }

    /// `bit14:0`: the type id, with the ignorable flag masked off.
    pub fn type_id(&self) -> u16 {
        self.type_raw & TYPE_ID_MASK
    }

    /// `true` if `type_id` falls in the spec-defined core range
    /// (0x0000-0x3FFF), as opposed to the experimental range
    /// (0x4000-0x7FFF).
    pub fn is_core(&self) -> bool {
        self.type_id() <= CORE_TYPE_ID_MAX
    }
}

/// Envelope parsing failed in a way that is not just "need more bytes".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    /// `length` exceeds the limit for the stream kind this Envelope is on
    /// (spec 2.1.1 table). Per spec this is a hard parse failure: the
    /// stream (or connection, for `control`) MUST be aborted.
    LengthExceedsLimit { length: u64, limit: u64 },
}

/// Encodes an Envelope (`length` + `type` + `payload`) onto the end of `out`.
///
/// `length` is `payload.len()` (spec 2.1.1, DR-032); callers are
/// responsible for ensuring `payload.len()` respects the destination
/// stream's length limit before calling this.
pub fn encode(type_raw: u16, payload: &[u8], out: &mut Vec<u8>) {
    varint::encode(payload.len() as u64, out);
    out.extend_from_slice(&type_raw.to_le_bytes());
    out.extend_from_slice(payload);
}

/// Attempts to decode one Envelope from the start of `buf`.
///
/// `max_length` is the length limit for the stream kind this data arrived
/// on (see [`crate::StreamKind::max_envelope_length`]).
///
/// Returns:
/// - `Ok(Some((envelope, consumed)))` on a complete, valid Envelope.
/// - `Ok(None)` if `buf` does not yet contain a complete Envelope (the
///   caller should buffer more bytes from the stream and retry).
/// - `Err(_)` if the declared `length` violates the stream's limit. This
///   check is performed as soon as `length` itself is decoded, without
///   waiting for the rest of the frame to arrive, so oversized frames are
///   rejected before they are buffered in full.
///
/// Never panics on attacker-controlled input.
pub fn parse<'a>(
    buf: &'a [u8],
    max_length: u64,
) -> Result<Option<(Envelope<'a>, usize)>, EnvelopeError> {
    let (length, length_len) = match varint::decode(buf) {
        Ok(v) => v,
        Err(varint::VarintError::Incomplete) => return Ok(None),
    };
    if length > max_length {
        return Err(EnvelopeError::LengthExceedsLimit {
            length,
            limit: max_length,
        });
    }
    // On a 32-bit target, an attacker-controlled varint up to 2^62-1
    // could exceed `usize::MAX`; the length check above already bounds
    // `length` to `max_length` (at most 16 MiB, spec 2.1.1), so this cast
    // is safe on every supported target, but assert defensively.
    let length = usize::try_from(length).expect("bounded by max_envelope_length");

    let header_len = length_len + 2;
    if buf.len() < header_len {
        return Ok(None);
    }
    let type_raw = u16::from_le_bytes([buf[length_len], buf[length_len + 1]]);

    let total_len = header_len + length;
    if buf.len() < total_len {
        return Ok(None);
    }
    let payload = &buf[header_len..total_len];

    Ok(Some((Envelope { type_raw, payload }, total_len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds raw envelope bytes: 1-byte-form varint length, LE type, payload.
    fn build(type_raw: u16, payload: &[u8]) -> Vec<u8> {
        assert!(
            payload.len() < 64,
            "test helper only supports 1-byte varint lengths"
        );
        let mut buf = vec![payload.len() as u8];
        buf.extend_from_slice(&type_raw.to_le_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    #[test]
    fn encode_matches_hand_built_bytes() {
        let mut out = Vec::new();
        encode(0x0001, b"hello", &mut out);
        assert_eq!(out, build(0x0001, b"hello"));
    }

    #[test]
    fn encode_decode_round_trip() {
        let mut out = Vec::new();
        encode(0x4002, b"payload bytes here", &mut out);
        let (env, consumed) = parse(&out, 1024).unwrap().unwrap();
        assert_eq!(consumed, out.len());
        assert_eq!(env.type_raw, 0x4002);
        assert_eq!(env.payload, b"payload bytes here");
    }

    #[test]
    fn encode_appends_without_clearing_existing_contents() {
        let mut out = vec![0xFF];
        encode(0x0001, b"x", &mut out);
        assert_eq!(out[0], 0xFF);
    }

    #[test]
    fn parses_core_type_envelope() {
        let buf = build(0x0001, b"hello");
        let (env, consumed) = parse(&buf, 1024).unwrap().unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(env.type_raw, 0x0001);
        assert_eq!(env.payload, b"hello");
        assert!(!env.is_ignorable());
        assert!(env.is_core());
        assert_eq!(env.type_id(), 0x0001);
    }

    #[test]
    fn parses_experimental_ignorable_envelope() {
        // ignorable flag set + type id in experimental range
        let type_raw = IGNORABLE_FLAG | 0x4001;
        let buf = build(type_raw, b"vendor-data");
        let (env, _) = parse(&buf, 1024).unwrap().unwrap();
        assert!(env.is_ignorable());
        assert!(!env.is_core());
        assert_eq!(env.type_id(), 0x4001);
    }

    #[test]
    fn core_ignorable_flag_is_independent_of_type_range() {
        // Nothing stops a core type from also setting the ignorable flag;
        // the two bits are independent per spec 2.1.1.
        let type_raw = IGNORABLE_FLAG | 0x0002;
        let buf = build(type_raw, &[]);
        let (env, _) = parse(&buf, 1024).unwrap().unwrap();
        assert!(env.is_ignorable());
        assert!(env.is_core());
    }

    #[test]
    fn type_id_boundary_0x3fff_is_core() {
        let buf = build(0x3FFF, &[]);
        let (env, _) = parse(&buf, 1024).unwrap().unwrap();
        assert!(env.is_core());
    }

    #[test]
    fn type_id_boundary_0x4000_is_experimental() {
        let buf = build(0x4000, &[]);
        let (env, _) = parse(&buf, 1024).unwrap().unwrap();
        assert!(!env.is_core());
    }

    #[test]
    fn empty_payload_is_valid() {
        let buf = build(0x0005, &[]);
        let (env, consumed) = parse(&buf, 1024).unwrap().unwrap();
        assert_eq!(consumed, 3);
        assert!(env.payload.is_empty());
    }

    #[test]
    fn empty_buffer_is_incomplete() {
        assert_eq!(parse(&[], 1024), Ok(None));
    }

    #[test]
    fn incomplete_varint_length_is_incomplete() {
        // 2-byte-form prefix but only 1 byte present
        assert_eq!(parse(&[0x7b], 1024), Ok(None));
    }

    #[test]
    fn missing_type_bytes_is_incomplete() {
        // length = 5, but no type/payload bytes follow at all
        assert_eq!(parse(&[0x05], 1024), Ok(None));
    }

    #[test]
    fn one_type_byte_missing_is_incomplete() {
        // length = 5, only 1 of the 2 type bytes present
        assert_eq!(parse(&[0x05, 0xAA], 1024), Ok(None));
    }

    #[test]
    fn truncated_payload_is_incomplete() {
        let full = build(0x0001, b"hello");
        let truncated = &full[..full.len() - 2];
        assert_eq!(parse(truncated, 1024), Ok(None));
    }

    #[test]
    fn length_exceeding_stream_limit_is_rejected() {
        // Declares a 1000-byte payload on a stream whose limit is 10.
        let buf = build(0, &[]); // placeholder, we hand-build below
        let _ = buf;
        let mut raw = vec![];
        // varint for 1000: 2-byte form, top 2 bits = 01
        let v: u16 = 1000;
        raw.push(0x40 | ((v >> 8) as u8));
        raw.push((v & 0xFF) as u8);
        raw.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            parse(&raw, 10),
            Err(EnvelopeError::LengthExceedsLimit {
                length: 1000,
                limit: 10
            })
        );
    }

    #[test]
    fn length_exceeding_limit_is_rejected_even_if_bytes_not_yet_buffered() {
        // Only the varint length itself has arrived; type/payload have not.
        // The parser must still reject immediately rather than waiting for
        // more data, since it already knows the frame violates the limit.
        let mut raw = vec![];
        let v: u16 = 1000;
        raw.push(0x40 | ((v >> 8) as u8));
        raw.push((v & 0xFF) as u8);
        assert_eq!(
            parse(&raw, 10),
            Err(EnvelopeError::LengthExceedsLimit {
                length: 1000,
                limit: 10
            })
        );
    }

    #[test]
    fn length_exactly_at_limit_is_accepted() {
        let payload = vec![0xAB; 10];
        let buf = build(0x0001, &payload);
        let result = parse(&buf, 10).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn parses_two_consecutive_envelopes_from_one_buffer() {
        let mut buf = build(0x0001, b"first");
        buf.extend(build(0x0002, b"second-msg"));

        let (env1, consumed1) = parse(&buf, 1024).unwrap().unwrap();
        assert_eq!(env1.payload, b"first");

        let (env2, consumed2) = parse(&buf[consumed1..], 1024).unwrap().unwrap();
        assert_eq!(env2.payload, b"second-msg");
        assert_eq!(consumed1 + consumed2, buf.len());
    }

    #[test]
    fn does_not_consume_trailing_bytes_beyond_this_envelope() {
        let mut buf = build(0x0001, b"one");
        let first_len = buf.len();
        buf.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // unrelated trailing junk
        let (_, consumed) = parse(&buf, 1024).unwrap().unwrap();
        assert_eq!(consumed, first_len);
    }
}
