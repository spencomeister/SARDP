//! QUIC variable-length integer encoding (RFC 9000 Section 16).
//!
//! Note: this is independent of the little-endian byte order used for the
//! rest of the wire format (spec 2.1) -- the varint encoding is defined
//! entirely by RFC 9000 and carries its own big-endian-style bit packing.

/// Decoding a varint failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarintError {
    /// Not enough bytes in the buffer yet to complete the varint.
    Incomplete,
}

/// Attempts to decode one QUIC varint from the start of `buf`.
///
/// Returns `Ok((value, consumed_bytes))` on success, or
/// `Err(VarintError::Incomplete)` if `buf` does not yet contain a complete
/// varint. Never panics on attacker-controlled input.
pub fn decode(buf: &[u8]) -> Result<(u64, usize), VarintError> {
    let first = *buf.first().ok_or(VarintError::Incomplete)?;
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        return Err(VarintError::Incomplete);
    }
    let mut value = u64::from(first & 0x3F);
    for &byte in &buf[1..len] {
        value = (value << 8) | u64::from(byte);
    }
    Ok((value, len))
}

/// The largest value representable by a QUIC varint (2^62 - 1).
pub const MAX: u64 = (1 << 62) - 1;

/// Encodes `value` as a QUIC varint, appending it to `out`.
///
/// Chooses the shortest of the four wire lengths (1/2/4/8 bytes) that fits
/// `value`. Panics (debug and release) if `value > MAX`, since that is a
/// programmer error on the encoding side, not attacker-controlled input.
pub fn encode(value: u64, out: &mut Vec<u8>) {
    assert!(value <= MAX, "varint value {value} exceeds 2^62-1");
    if value < (1 << 6) {
        out.push(value as u8);
    } else if value < (1 << 14) {
        let v = value as u16;
        out.extend_from_slice(&[0x40 | (v >> 8) as u8, v as u8]);
    } else if value < (1 << 30) {
        let v = value as u32;
        out.extend_from_slice(&(0x8000_0000 | v).to_be_bytes());
    } else {
        out.extend_from_slice(&(0xC000_0000_0000_0000 | value).to_be_bytes());
    }
}

#[cfg(test)]
mod encode_tests {
    use super::*;

    #[test]
    fn round_trips_boundary_values() {
        for value in [
            0,
            1,
            63,            // max 1-byte
            64,            // min 2-byte
            16_383,        // max 2-byte
            16_384,        // min 4-byte
            1_073_741_823, // max 4-byte
            1_073_741_824, // min 8-byte
            MAX,
        ] {
            let mut buf = Vec::new();
            encode(value, &mut buf);
            let (decoded, consumed) = decode(&buf).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn chooses_shortest_encoding() {
        let mut buf = Vec::new();
        encode(37, &mut buf);
        assert_eq!(buf, vec![0x25]);
    }

    #[test]
    fn matches_rfc9000_2_byte_example() {
        let mut buf = Vec::new();
        encode(15293, &mut buf);
        assert_eq!(buf, vec![0x7b, 0xbd]);
    }

    #[test]
    #[should_panic]
    fn panics_on_value_exceeding_max() {
        let mut buf = Vec::new();
        encode(MAX + 1, &mut buf);
    }

    #[test]
    fn appends_without_clearing_existing_contents() {
        let mut buf = vec![0xAA, 0xBB];
        encode(37, &mut buf);
        assert_eq!(buf, vec![0xAA, 0xBB, 0x25]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_1_byte_form() {
        // RFC 9000 Appendix A.1 example: 0x25 -> 37, 1 byte
        assert_eq!(decode(&[0x25]), Ok((37, 1)));
    }

    #[test]
    fn decodes_2_byte_form() {
        // 0x7bbd -> 15293, 2 bytes
        assert_eq!(decode(&[0x7b, 0xbd]), Ok((15293, 2)));
    }

    #[test]
    fn decodes_4_byte_form() {
        // 0x9d7f3e7d -> 494878333, 4 bytes
        assert_eq!(decode(&[0x9d, 0x7f, 0x3e, 0x7d]), Ok((494_878_333, 4)));
    }

    #[test]
    fn decodes_8_byte_form() {
        // 0xc2197c5eff14e88c -> 151288809941952652, 8 bytes
        assert_eq!(
            decode(&[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c]),
            Ok((151_288_809_941_952_652, 8))
        );
    }

    #[test]
    fn decodes_zero() {
        assert_eq!(decode(&[0x00]), Ok((0, 1)));
    }

    #[test]
    fn empty_buffer_is_incomplete() {
        assert_eq!(decode(&[]), Err(VarintError::Incomplete));
    }

    #[test]
    fn truncated_multi_byte_form_is_incomplete() {
        // Prefix bits say 4-byte form, but only 2 bytes are present.
        assert_eq!(decode(&[0x9d, 0x7f]), Err(VarintError::Incomplete));
    }

    #[test]
    fn truncated_8_byte_form_is_incomplete() {
        assert_eq!(decode(&[0xc2, 0x19, 0x7c]), Err(VarintError::Incomplete));
    }

    #[test]
    fn leaves_trailing_bytes_unconsumed() {
        let buf = [0x25, 0xAA, 0xBB];
        assert_eq!(decode(&buf), Ok((37, 1)));
    }

    #[test]
    fn max_1_byte_value() {
        // top 2 bits 00, remaining 6 bits all set = 0x3F = 63
        assert_eq!(decode(&[0x3F]), Ok((63, 1)));
    }

    #[test]
    fn max_8_byte_value() {
        // 0xC0 with all following bits set -> 2^62 - 1
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(decode(&buf), Ok(((1u64 << 62) - 1, 8)));
    }
}
