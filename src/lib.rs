//! SARDP wire-format core (M1): the hand-written, fuzz-target Envelope and
//! StreamPrologue parsers (spec 2.1.1, 2.2), per DR-021's exception to the
//! "no hand-written parsers" rule.

pub mod envelope;
pub mod prologue;
pub mod stream_kind;
pub mod varint;

pub use envelope::{Envelope, EnvelopeError};
pub use prologue::{PrologueError, StreamPrologue};
pub use stream_kind::StreamKind;
