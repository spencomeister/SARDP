//! Fuzzes `sardp::envelope::parse` (Part 8's MUST: "Envelopeパーサーは
//! 手書きの最小実装とし、MUSTでファジング対象に含める").
//!
//! The first byte selects one of the spec 2.1.1 stream-kind length limits
//! (so the fuzzer explores both "well within limit" and "limit exceeded"
//! paths realistically, rather than fuzzing against one fixed bound); the
//! rest of the input is the buffer handed to `parse` as if it had just
//! arrived on a stream. `parse`'s own doc comment states it never panics
//! on attacker-controlled input -- this target's only job is to prove
//! that under libFuzzer's coverage-guided search, not just the crate's
//! example-based unit tests.

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

fuzz_target!(|data: &[u8]| {
    let Some((&selector, buf)) = data.split_first() else {
        return;
    };
    let max_length = KINDS[selector as usize % KINDS.len()].max_envelope_length();
    let _ = envelope::parse(buf, max_length);
});
