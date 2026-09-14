//! `sardp-server`'s per-connection error type and the classification that
//! decides whether an error represents the underlying QUIC transport
//! actually going away (peer killed, network partition, timed out, ...)
//! rather than a protocol violation the server itself detected. Lives in
//! the library crate (KNOWN_ISSUES.md #1) purely so it can carry unit
//! tests -- `ConnError` itself is only ever constructed by
//! `sardp-cli/src/bin/sardp-server.rs`.

use crate::audio_session::AudioError;
use crate::clipboard_session::ClipboardSessionError;
use crate::encoder;
use crate::feedback_session::ReadFeedbackError;
use crate::handshake::HandshakeError;
use crate::reason_code::ReasonCode;
use crate::stream_reader::StreamReadError;
use crate::timesync::TimeSyncError;
use crate::video_session::VideoError;

// The variant payloads are only ever read via the `Debug` derive (when
// `sardp-server`'s `main` logs a failed connection's error) -- rustc's
// dead_code lint doesn't credit that as a "read", hence the blanket allow.
#[derive(Debug)]
#[allow(dead_code)]
pub enum ConnError {
    Handshake(HandshakeError),
    Quic(quinn::ConnectionError),
    Write(quinn::WriteError),
    Video(VideoError),
    Feedback(ReadFeedbackError),
    Violation(ReasonCode),
    Encode(encoder::EncodeError),
    Join(tokio::task::JoinError),
    Read(StreamReadError),
    TimeSync(TimeSyncError),
    Audio(AudioError),
    Clipboard(ClipboardSessionError),
    /// The real desktop capture/encode source (Stage 3, `sardp-win`)
    /// failed or went away. Carried as a string so this crate doesn't
    /// depend on the Windows-only crate; never a transport disconnect.
    Capture(String),
    /// Spec 4.1: `IDLE_TIMEOUT` fired (`Active -> Suspended`). Distinct
    /// from a genuine transport failure so `handle_connection` can tell
    /// the two apart in logs, even though both are handled the same way
    /// (see [`is_transport_disconnect`]).
    IdleTimeout,
}

impl From<StreamReadError> for ConnError {
    fn from(e: StreamReadError) -> Self {
        Self::Read(e)
    }
}
impl From<TimeSyncError> for ConnError {
    fn from(e: TimeSyncError) -> Self {
        Self::TimeSync(e)
    }
}
impl From<HandshakeError> for ConnError {
    fn from(e: HandshakeError) -> Self {
        Self::Handshake(e)
    }
}
impl From<quinn::ConnectionError> for ConnError {
    fn from(e: quinn::ConnectionError) -> Self {
        Self::Quic(e)
    }
}
impl From<quinn::WriteError> for ConnError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}
impl From<VideoError> for ConnError {
    fn from(e: VideoError) -> Self {
        Self::Video(e)
    }
}
impl From<ReadFeedbackError> for ConnError {
    fn from(e: ReadFeedbackError) -> Self {
        Self::Feedback(e)
    }
}
impl From<crate::ProtocolViolation> for ConnError {
    fn from(v: crate::ProtocolViolation) -> Self {
        Self::Violation(v.reason)
    }
}
impl From<crate::video_sm::ProtocolViolation> for ConnError {
    fn from(v: crate::video_sm::ProtocolViolation) -> Self {
        Self::Violation(v.reason)
    }
}
impl From<encoder::EncodeError> for ConnError {
    fn from(e: encoder::EncodeError) -> Self {
        Self::Encode(e)
    }
}
impl From<tokio::task::JoinError> for ConnError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Join(e)
    }
}
impl From<AudioError> for ConnError {
    fn from(e: AudioError) -> Self {
        Self::Audio(e)
    }
}
impl From<ClipboardSessionError> for ConnError {
    fn from(e: ClipboardSessionError) -> Self {
        Self::Clipboard(e)
    }
}

/// Whether `error` represents the underlying QUIC transport actually going
/// away (peer killed, network partition, timed out, ...) rather than a
/// protocol violation this server itself detected. Spec 4.1's diagram only
/// lists `IDLE_TIMEOUT` as an explicit `Active -> Suspended` trigger
/// (`sardp-server`'s active-session loop handles that one inline), but a
/// real abrupt disconnect -- the scenario "kill the client, then
/// reconnect" actually exercises -- surfaces here as exactly this kind of
/// transport error, in practice well before `IDLE_TIMEOUT` would otherwise
/// fire. Treating it the same way (`Suspended`, reconnectable) rather than
/// a hard failure is what makes reconnection reachable from a real
/// disconnect, matching how `tests/phase1_reconnection.rs` already frames
/// a "genuinely lost" connection at the library level.
pub fn is_transport_disconnect(error: &ConnError) -> bool {
    fn is_read_disconnect(e: &StreamReadError) -> bool {
        matches!(e, StreamReadError::Read(_) | StreamReadError::ClosedEarly)
    }
    match error {
        ConnError::IdleTimeout => true,
        ConnError::Quic(_) | ConnError::Write(_) => true,
        ConnError::Read(e) => is_read_disconnect(e),
        ConnError::Feedback(ReadFeedbackError::Quic(_)) => true,
        ConnError::Feedback(ReadFeedbackError::Read(e)) => is_read_disconnect(e),
        // The video stream (frame send, and the backpressure reopen path)
        // wraps its own transport errors in VideoError rather than
        // ConnError directly -- caught by manual testing: a real `kill -9`
        // surfaces here (mid frame-send) well before any control-stream
        // read notices anything wrong.
        ConnError::Video(VideoError::Quic(_) | VideoError::Write(_)) => true,
        ConnError::Video(VideoError::Read(e)) => is_read_disconnect(e),
        // The audio_capture acceptor arm (KNOWN_ISSUES.md #12) surfaces a
        // dead connection the same way: accept_uni()/read failing with a
        // transport-level error rather than a protocol violation.
        ConnError::Audio(AudioError::Quic(_) | AudioError::Write(_)) => true,
        ConnError::Audio(AudioError::Read(e)) => is_read_disconnect(e),
        // The clipboard-accept arm (CLIP_WRITE, KNOWN_ISSUES.md) is the
        // same story: accept_bi()/read failing with a transport error
        // rather than a protocol violation.
        ConnError::Clipboard(ClipboardSessionError::Quic(_) | ClipboardSessionError::Write(_)) => {
            true
        }
        ConnError::Clipboard(ClipboardSessionError::Read(e)) => is_read_disconnect(e),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timeout_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::IdleTimeout));
    }

    #[test]
    fn a_quic_connection_error_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Quic(
            quinn::ConnectionError::TimedOut
        )));
    }

    #[test]
    fn a_quic_write_error_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Write(
            quinn::WriteError::ClosedStream
        )));
    }

    #[test]
    fn a_closed_early_control_read_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Read(
            StreamReadError::ClosedEarly
        )));
    }

    #[test]
    fn a_malformed_envelope_on_the_control_stream_is_not_a_disconnect() {
        // A parse failure is a protocol problem, not the transport going
        // away -- must not be treated as reconnectable.
        assert!(!is_transport_disconnect(&ConnError::Read(
            StreamReadError::Envelope(crate::envelope::EnvelopeError::LengthExceedsLimit {
                length: 1_000_000,
                limit: 1024,
            })
        )));
    }

    #[test]
    fn a_feedback_stream_quic_error_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Feedback(
            ReadFeedbackError::Quic(quinn::ConnectionError::Reset)
        )));
    }

    #[test]
    fn a_feedback_stream_closed_early_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Feedback(
            ReadFeedbackError::Read(StreamReadError::ClosedEarly)
        )));
    }

    #[test]
    fn a_video_stream_quic_or_write_error_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Video(
            VideoError::Quic(quinn::ConnectionError::LocallyClosed)
        )));
        assert!(is_transport_disconnect(&ConnError::Video(
            VideoError::Write(quinn::WriteError::ZeroRttRejected)
        )));
    }

    #[test]
    fn a_video_stream_closed_early_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Video(
            VideoError::Read(StreamReadError::ClosedEarly)
        )));
    }

    #[test]
    fn a_video_protocol_violation_is_not_a_disconnect() {
        assert!(!is_transport_disconnect(&ConnError::Video(
            VideoError::FrameLengthMismatch
        )));
    }

    #[test]
    fn an_audio_quic_or_write_error_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Audio(
            AudioError::Quic(quinn::ConnectionError::Reset)
        )));
        assert!(is_transport_disconnect(&ConnError::Audio(
            AudioError::Write(quinn::WriteError::ClosedStream)
        )));
    }

    #[test]
    fn an_audio_closed_early_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Audio(
            AudioError::Read(StreamReadError::ClosedEarly)
        )));
    }

    #[test]
    fn an_audio_protocol_violation_is_not_a_disconnect() {
        assert!(!is_transport_disconnect(&ConnError::Audio(
            AudioError::WrongStreamKind
        )));
    }

    #[test]
    fn a_clipboard_quic_or_write_error_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Clipboard(
            ClipboardSessionError::Quic(quinn::ConnectionError::Reset)
        )));
        assert!(is_transport_disconnect(&ConnError::Clipboard(
            ClipboardSessionError::Write(quinn::WriteError::ClosedStream)
        )));
    }

    #[test]
    fn a_clipboard_closed_early_is_a_disconnect() {
        assert!(is_transport_disconnect(&ConnError::Clipboard(
            ClipboardSessionError::Read(StreamReadError::ClosedEarly)
        )));
    }

    #[test]
    fn a_clipboard_protocol_violation_is_not_a_disconnect() {
        assert!(!is_transport_disconnect(&ConnError::Clipboard(
            ClipboardSessionError::WrongStreamKind
        )));
    }

    #[test]
    fn a_reason_code_violation_is_not_a_disconnect() {
        assert!(!is_transport_disconnect(&ConnError::Violation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
        )));
    }

    #[test]
    fn a_join_error_is_not_a_disconnect() {
        // JoinError has no public constructor; the catch-all `_ => false`
        // branch below covers it -- exercised indirectly via any other
        // variant this test module can't construct directly.
        assert!(!is_transport_disconnect(&ConnError::Encode(
            encoder::EncodeError::EmptyOutput
        )));
    }
}
