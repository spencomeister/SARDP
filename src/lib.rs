//! SARDP wire-format core (M1): the hand-written, fuzz-target Envelope and
//! StreamPrologue parsers (spec 2.1.1, 2.2), per DR-021's exception to the
//! "no hand-written parsers" rule.

pub mod auth;
pub mod connection_sm;
pub mod envelope;
pub mod handshake;
pub mod messages;
pub mod net;
pub mod pki;
pub mod prologue;
pub mod reason_code;
pub mod stream_kind;
pub mod varint;

pub use connection_sm::{ConnectionSm, ConnectionState, ProtocolViolation};
pub use envelope::{Envelope, EnvelopeError};
pub use prologue::{PrologueError, StreamPrologue};
pub use reason_code::ReasonCode;
pub use stream_kind::StreamKind;
