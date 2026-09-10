//! Fuzzes `sardp::prologue::parse`: the other hand-written, minimal parser
//! in this crate (spec 2.2, DR-015 -- not Envelope-wrapped, so it can't go
//! through the DR-021 schema-generated-code path either). Part 8 names
//! the Envelope parser specifically for fuzzing (`envelope_parse.rs`), but
//! `StreamPrologue` is the same class of hand-written, attacker-facing
//! parser (it's the very first bytes read off any newly opened stream),
//! so it gets the same treatment here.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sardp::prologue;

fuzz_target!(|data: &[u8]| {
    let _ = prologue::parse(data);
});
