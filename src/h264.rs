//! Minimal H.264 Annex-B NAL unit splitting, just enough to verify the
//! self-containment rule in spec 2.10: "IDRフレームのpayloadはMUSTでSPS/PPS
//! NALユニットを含み、単体で自己完結的にデコード可能でなければならない".
//!
//! This is not a general-purpose H.264 parser (no RBSP de-escaping, no
//! slice header parsing) -- just enough structure to assert "this Annex-B
//! byte stream contains an SPS, a PPS, and an IDR slice, with the SPS/PPS
//! preceding the slice" in tests, without needing a full decoder.

/// One NAL unit as found in an Annex-B byte stream: `nal_type` (bits 4:0
/// of the NAL header byte) and the NAL's payload bytes (header included,
/// start code excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalUnit<'a> {
    pub nal_type: u8,
    pub bytes: &'a [u8],
}

pub const NAL_TYPE_SLICE_IDR: u8 = 5;
pub const NAL_TYPE_SPS: u8 = 7;
pub const NAL_TYPE_PPS: u8 = 8;

/// Finds the next Annex-B start code (`00 00 01` or `00 00 00 01`) at or
/// after `from`, returning `(start_code_offset, nal_start_offset)`.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                return Some((i, i + 3));
            }
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some((i, i + 4));
            }
        }
        i += 1;
    }
    None
}

/// Splits an Annex-B byte stream into NAL units. Ignores any leading
/// bytes before the first start code. Never panics on malformed input
/// (yields no more units once no further start code is found).
pub fn split_annex_b(buf: &[u8]) -> Vec<NalUnit<'_>> {
    let mut units = Vec::new();
    let Some((_, mut nal_start)) = find_start_code(buf, 0) else {
        return units;
    };
    loop {
        let next = find_start_code(buf, nal_start);
        let nal_end = next.map(|(sc_start, _)| sc_start).unwrap_or(buf.len());
        if nal_end > nal_start {
            let bytes = &buf[nal_start..nal_end];
            let nal_type = bytes[0] & 0x1F;
            units.push(NalUnit { nal_type, bytes });
        }
        match next {
            Some((_, next_nal_start)) => nal_start = next_nal_start,
            None => break,
        }
    }
    units
}

/// Checks spec 2.10's IDR self-containment rule: the byte stream contains
/// an SPS and a PPS, both before the first IDR slice NAL.
pub fn is_self_contained_idr(buf: &[u8]) -> bool {
    let units = split_annex_b(buf);
    let mut seen_sps = false;
    let mut seen_pps = false;
    for unit in units {
        match unit.nal_type {
            NAL_TYPE_SPS => seen_sps = true,
            NAL_TYPE_PPS => seen_pps = true,
            NAL_TYPE_SLICE_IDR => return seen_sps && seen_pps,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(start_code: &[u8], nal_type: u8, extra: &[u8]) -> Vec<u8> {
        let mut v = start_code.to_vec();
        v.push(nal_type); // nal_ref_idc=0 in top 3 bits, doesn't matter for this test
        v.extend_from_slice(extra);
        v
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let mut buf = nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0xAA]);
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_PPS, &[0xBB]));
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC, 0xDD]));

        let units = split_annex_b(&buf);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].nal_type, NAL_TYPE_SPS);
        assert_eq!(units[1].nal_type, NAL_TYPE_PPS);
        assert_eq!(units[2].nal_type, NAL_TYPE_SLICE_IDR);
        assert_eq!(units[2].bytes, &[NAL_TYPE_SLICE_IDR, 0xCC, 0xDD]);
    }

    #[test]
    fn empty_buffer_yields_no_units() {
        assert!(split_annex_b(&[]).is_empty());
    }

    #[test]
    fn buffer_without_start_code_yields_no_units() {
        assert!(split_annex_b(&[1, 2, 3, 4]).is_empty());
    }

    #[test]
    fn self_contained_idr_detects_sps_pps_before_idr() {
        let mut buf = nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0xAA]);
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_PPS, &[0xBB]));
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC]));
        assert!(is_self_contained_idr(&buf));
    }

    #[test]
    fn idr_without_sps_pps_is_not_self_contained() {
        let buf = nal(&[0, 0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC]);
        assert!(!is_self_contained_idr(&buf));
    }

    #[test]
    fn idr_before_sps_pps_is_not_self_contained() {
        // pathological ordering: IDR arrives before its own parameter sets
        let mut buf = nal(&[0, 0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC]);
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_SPS, &[0xAA]));
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_PPS, &[0xBB]));
        assert!(!is_self_contained_idr(&buf));
    }

    #[test]
    fn sps_only_without_pps_is_not_self_contained() {
        let mut buf = nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0xAA]);
        buf.extend(nal(&[0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC]));
        assert!(!is_self_contained_idr(&buf));
    }

    #[test]
    fn leading_bytes_before_first_start_code_are_ignored() {
        let mut buf = vec![0xDE, 0xAD, 0xBE, 0xEF];
        buf.extend(nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0xAA]));
        let units = split_annex_b(&buf);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].nal_type, NAL_TYPE_SPS);
    }
}
