//! Minimal H.264 Annex-B NAL unit handling, shared by every platform's
//! encode/decode path (Windows: `sardp-win`; macOS/Linux: to come).
//!
//! Two jobs, both born of hardware encoders whose *metadata* about their
//! own output can't be trusted (3W-1-b/d-2 finding on the NVIDIA H.264
//! Encoder MFT: `MFSampleExtension_CleanPoint` was set on the first IDR
//! only, and SPS/PPS were attached to the first IDR only):
//!
//! - Classify an access unit from its NAL units rather than from the
//!   encoder's keyframe flag ([`is_idr_access_unit`], [`nal_unit_types`]).
//! - Keep an IDR self-contained as spec 2.10 requires ("IDRフレームのpayloadは
//!   MUSTでSPS/PPS NALユニットを含み、単体で自己完結的にデコード可能でなければ
//!   ならない") by caching the parameter sets from whichever access unit
//!   carried them and prepending them to later IDRs that lack them
//!   ([`ParameterSetCache`]), with [`is_self_contained_idr`] as the check.
//!
//! This is not a general-purpose H.264 parser (no RBSP de-escaping, no
//! slice header parsing) -- just start-code splitting and NAL header
//! types, which is all the above needs. Never panics on malformed input.

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

/// Byte spans of every NAL unit: `(start_code_offset, nal_start_offset,
/// end_offset)`, so a caller can copy a unit *with* its original start
/// code (`buf[start_code..end]`) or without (`buf[nal_start..end]`).
/// Ignores any leading bytes before the first start code; an empty unit
/// (two adjacent start codes) is skipped.
fn nal_spans(buf: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    let Some((mut sc_start, mut nal_start)) = find_start_code(buf, 0) else {
        return spans;
    };
    loop {
        let next = find_start_code(buf, nal_start);
        let end = next.map(|(sc, _)| sc).unwrap_or(buf.len());
        if end > nal_start {
            spans.push((sc_start, nal_start, end));
        }
        match next {
            Some((next_sc, next_nal)) => {
                sc_start = next_sc;
                nal_start = next_nal;
            }
            None => break,
        }
    }
    spans
}

/// Splits an Annex-B byte stream into NAL units. Ignores any leading
/// bytes before the first start code. Never panics on malformed input
/// (yields no more units once no further start code is found).
pub fn split_annex_b(buf: &[u8]) -> Vec<NalUnit<'_>> {
    nal_spans(buf)
        .into_iter()
        .map(|(_, nal_start, end)| {
            let bytes = &buf[nal_start..end];
            NalUnit {
                nal_type: bytes[0] & 0x1F,
                bytes,
            }
        })
        .collect()
}

/// `nal_unit_type` of every NAL unit, in stream order.
pub fn nal_unit_types(buf: &[u8]) -> Vec<u8> {
    split_annex_b(buf).into_iter().map(|u| u.nal_type).collect()
}

/// Whether the access unit carries an IDR slice (NAL type 5).
pub fn contains_idr_slice(buf: &[u8]) -> bool {
    split_annex_b(buf)
        .into_iter()
        .any(|u| u.nal_type == NAL_TYPE_SLICE_IDR)
}

/// Whether the access unit contains an SPS (NAL type 7).
pub fn contains_sps(buf: &[u8]) -> bool {
    split_annex_b(buf)
        .into_iter()
        .any(|u| u.nal_type == NAL_TYPE_SPS)
}

/// The IDR decision for an encoder's output: the encoder's own keyframe
/// flag OR an IDR slice actually present in the bytes. Hardware encoders
/// have been seen to set their flag on the first IDR only, so the bytes
/// are the authority; the flag is kept as a belt-and-braces input for an
/// encoder that emits an IDR the scan somehow misses.
pub fn is_idr_access_unit(buf: &[u8], encoder_keyframe_flag: bool) -> bool {
    encoder_keyframe_flag || contains_idr_slice(buf)
}

/// The SPS (7) and PPS (8) NAL units, start codes included, concatenated
/// in stream order -- i.e. exactly what has to precede an IDR slice for
/// the access unit to be self-contained. `None` if either is missing.
pub fn extract_parameter_sets(buf: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut sps, mut pps) = (false, false);
    for (sc_start, nal_start, end) in nal_spans(buf) {
        match buf[nal_start] & 0x1F {
            NAL_TYPE_SPS => {
                sps = true;
                out.extend_from_slice(&buf[sc_start..end]);
            }
            NAL_TYPE_PPS => {
                pps = true;
                out.extend_from_slice(&buf[sc_start..end]);
            }
            _ => {}
        }
    }
    (sps && pps).then_some(out)
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

/// Remembers a stream's SPS+PPS (Annex-B, start codes included) so every
/// IDR the encoder emits *without* them can be made self-contained.
///
/// Feed every access unit to [`Self::observe`]; the first one carrying
/// both parameter sets fills the cache. An encoder that never puts them
/// in-band can supply them through [`Self::set_fallback`] (e.g. Media
/// Foundation's `MF_MT_MPEG_SEQUENCE_HEADER`, or an `avcC` record
/// converted to Annex-B on macOS). Then [`Self::complete_idr`] prepends
/// them to an IDR that lacks an SPS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParameterSetCache {
    sets: Option<Vec<u8>>,
}

impl ParameterSetCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_sets(&self) -> bool {
        self.sets.is_some()
    }

    /// The cached SPS+PPS, if any.
    pub fn sets(&self) -> Option<&[u8]> {
        self.sets.as_deref()
    }

    /// Caches the parameter sets from `access_unit` if it carries both and
    /// nothing is cached yet. Returns whether the cache is filled after
    /// the call.
    pub fn observe(&mut self, access_unit: &[u8]) -> bool {
        if self.sets.is_none() {
            self.sets = extract_parameter_sets(access_unit);
        }
        self.sets.is_some()
    }

    /// Supplies out-of-band parameter sets (already Annex-B with start
    /// codes); only used if nothing in-band has been cached yet.
    pub fn set_fallback(&mut self, sets: Vec<u8>) {
        if self.sets.is_none() && !sets.is_empty() {
            self.sets = Some(sets);
        }
    }

    /// Makes an IDR access unit self-contained: if it has no SPS and the
    /// cache has one, the cached SPS+PPS are prepended. Anything else
    /// (already self-contained, or nothing cached) is returned unchanged.
    pub fn complete_idr(&self, access_unit: Vec<u8>) -> Vec<u8> {
        match &self.sets {
            Some(sets) if !contains_sps(&access_unit) => {
                let mut out = Vec::with_capacity(sets.len() + access_unit.len());
                out.extend_from_slice(sets);
                out.extend_from_slice(&access_unit);
                out
            }
            _ => access_unit,
        }
    }
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

    // --- shared with sardp-win since 3W-1 (moved here for macOS reuse) ---

    #[test]
    fn nal_unit_types_handles_both_start_code_lengths() {
        let buf = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC];
        assert_eq!(nal_unit_types(&buf), vec![7, 8, 5]);
        assert_eq!(nal_unit_types(&[0, 0, 0, 0]), Vec::<u8>::new());
        assert_eq!(nal_unit_types(&[]), Vec::<u8>::new());
    }

    #[test]
    fn extract_parameter_sets_keeps_sps_and_pps_with_their_start_codes() {
        let sps = [0, 0, 0, 1, 0x67, 0xAA, 0xAB];
        let pps = [0, 0, 1, 0x68, 0xBB];
        let idr = [0, 0, 0, 1, 0x65, 0xCC, 0xCD];
        let mut buf = Vec::new();
        buf.extend_from_slice(&sps);
        buf.extend_from_slice(&pps);
        buf.extend_from_slice(&idr);
        let mut expected = Vec::new();
        expected.extend_from_slice(&sps);
        expected.extend_from_slice(&pps);
        assert_eq!(extract_parameter_sets(&buf), Some(expected));
        assert_eq!(extract_parameter_sets(&idr), None);
        // SPS without PPS isn't a usable pair.
        let mut sps_only = sps.to_vec();
        sps_only.extend_from_slice(&idr);
        assert_eq!(extract_parameter_sets(&sps_only), None);
    }

    #[test]
    fn idr_decision_trusts_the_bytes_over_the_encoder_flag() {
        let idr = nal(&[0, 0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC]);
        let p = nal(&[0, 0, 0, 1], 1, &[0xCC]); // non-IDR slice
        assert!(is_idr_access_unit(&idr, false));
        assert!(!is_idr_access_unit(&p, false));
        // The flag can only add, never remove.
        assert!(is_idr_access_unit(&p, true));
    }

    #[test]
    fn parameter_set_cache_completes_later_idrs() {
        let mut cache = ParameterSetCache::new();
        assert!(!cache.has_sets());

        let mut first_idr = nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0xAA]);
        first_idr.extend(nal(&[0, 0, 1], NAL_TYPE_PPS, &[0xBB]));
        first_idr.extend(nal(&[0, 0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xCC]));
        assert!(cache.observe(&first_idr));
        // Already self-contained: untouched.
        assert_eq!(cache.complete_idr(first_idr.clone()), first_idr);

        // A P-frame doesn't disturb the cache.
        let p = nal(&[0, 0, 0, 1], 1, &[0xDD]);
        assert!(cache.observe(&p));

        // A bare IDR (the hardware encoder's usual later IDR) gets the
        // sets prepended and becomes spec-2.10 self-contained.
        let bare_idr = nal(&[0, 0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xEE]);
        assert!(!is_self_contained_idr(&bare_idr));
        let completed = cache.complete_idr(bare_idr.clone());
        assert!(is_self_contained_idr(&completed));
        assert_eq!(nal_unit_types(&completed), vec![7, 8, 5]);
        assert!(completed.ends_with(&bare_idr));
    }

    #[test]
    fn parameter_set_cache_uses_the_fallback_only_when_nothing_is_in_band() {
        let mut cache = ParameterSetCache::new();
        let bare_idr = nal(&[0, 0, 0, 1], NAL_TYPE_SLICE_IDR, &[0xEE]);
        // Nothing cached: the IDR stays as it is (and the caller can log it).
        assert_eq!(cache.complete_idr(bare_idr.clone()), bare_idr);
        assert!(!cache.observe(&bare_idr));

        let mut oob = nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0x11]);
        oob.extend(nal(&[0, 0, 0, 1], NAL_TYPE_PPS, &[0x22]));
        cache.set_fallback(oob.clone());
        assert_eq!(cache.sets(), Some(oob.as_slice()));
        assert!(is_self_contained_idr(&cache.complete_idr(bare_idr.clone())));

        // In-band sets seen first win over a later fallback.
        let mut cache = ParameterSetCache::new();
        let mut in_band = nal(&[0, 0, 0, 1], NAL_TYPE_SPS, &[0xAA]);
        in_band.extend(nal(&[0, 0, 1], NAL_TYPE_PPS, &[0xBB]));
        cache.observe(&in_band);
        cache.set_fallback(oob);
        assert_eq!(cache.sets(), Some(in_band.as_slice()));
        // An empty fallback is ignored.
        let mut empty = ParameterSetCache::new();
        empty.set_fallback(Vec::new());
        assert!(!empty.has_sets());
    }
}
