//! SARDP wire-format core (M1): the hand-written, fuzz-target Envelope and
//! StreamPrologue parsers (spec 2.1.1, 2.2), per DR-021's exception to the
//! "no hand-written parsers" rule.

pub mod auth;
pub mod backpressure;
pub mod channel_sm;
pub mod client_display;
pub mod clock;
pub mod connection_sm;
pub mod decoder;
pub mod dev_identity;
pub mod encoder;
pub mod envelope;
pub mod feedback_session;
pub mod h264;
pub mod handshake;
pub mod measurement;
pub mod messages;
pub mod net;
pub mod netem;
pub mod permission_set;
pub mod permission_sm;
pub mod pki;
pub mod prologue;
pub mod reason_code;
pub mod reconnection;
pub mod session_store;
pub mod stream_kind;
pub mod stream_reader;
pub mod timecode_frame;
pub mod timesync;
pub mod varint;
pub mod video_channel;
pub mod video_session;
pub mod video_sm;

pub use connection_sm::{ConnectionSm, ConnectionState, ProtocolViolation};
pub use envelope::{Envelope, EnvelopeError};
pub use prologue::{PrologueError, StreamPrologue};
pub use reason_code::ReasonCode;
pub use stream_kind::StreamKind;
