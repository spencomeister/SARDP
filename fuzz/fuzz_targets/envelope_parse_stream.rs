//! Fuzzes the *repeated*-parse loop pattern `stream_reader::EnvelopeReader`
//! actually uses (buffer more bytes, call `parse`, advance past
//! `consumed`, repeat) rather than a single isolated `parse` call. This
//! catches a different class of bug than `envelope_parse.rs`: one where an
//! individual call is safe in isolation but `consumed` misbehaves across
//! repeated calls over the same growing buffer (e.g. failing to make
//! progress, or advancing past the end of what was actually validated).
//!
//! The iteration cap exists only to keep the fuzz target itself
//! terminating; hitting it would itself be a finding (parse stopped
//! making progress on a non-empty buffer without returning `Ok(None)`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use sardp::StreamKind;
use sardp::envelope;

const KINDS: [StreamKind; 8] = [
    StreamKind::Control,
    StreamKind::Input,
    StreamKind::Video,
    StreamKind::Feedback,
    StreamKind::Clipboard,
    StreamKind::File,
    StreamKind::AudioPlayback,
    StreamKind::AudioCapture,
];

const MAX_ITERATIONS: usize = 10_000;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, mut buf)) = data.split_first() else {
        return;
    };
    let max_length = KINDS[selector as usize % KINDS.len()].max_envelope_length();

    for _ in 0..MAX_ITERATIONS {
        match envelope::parse(buf, max_length) {
            Ok(Some((_envelope, consumed))) => {
                assert!(
                    consumed > 0 && consumed <= buf.len(),
                    "parse must consume between 1 and buf.len() bytes on success"
                );
                buf = &buf[consumed..];
                if buf.is_empty() {
                    return;
                }
            }
            Ok(None) => return,
            Err(_) => return,
        }
    }
    panic!("parse did not terminate within {MAX_ITERATIONS} iterations");
});
