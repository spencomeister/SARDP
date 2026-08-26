//! `feedback` stream (spec 2.14, 2.2.1: client-initiated, unidirectional
//! client->server) and `TransportFeedback` construction.
//!
//! Sends one feedback message per call; the spec's 100ms periodic
//! schedule (plus on `last_displayed_frame_id` change, spec 2.14) is left
//! to the caller (a real client would drive this from a timer loop, which
//! fits better alongside M5's backpressure work that needs the same
//! stream kept continuously fed).

use crate::messages::{self, TransportFeedback, VideoFrameHeader};
use crate::prologue;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};
use crate::timesync;

#[derive(Debug)]
pub enum ReadFeedbackError {
    Quic(quinn::ConnectionError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    WrongStreamKind,
    UnexpectedType(u16),
}

/// Opens the `feedback` stream (spec 2.2.1: `context_id` unused, 0).
pub async fn open_feedback_stream(
    connection: &quinn::Connection,
) -> Result<quinn::SendStream, quinn::ConnectionError> {
    let mut send = connection.open_uni().await?;
    let mut prologue_bytes = Vec::new();
    prologue::encode(StreamKind::Feedback, 1, 0, &mut prologue_bytes);
    // A write error here would surface on the next real send below; for
    // this PoC's one-shot-per-call usage there's no separate error path
    // worth adding just for the prologue write.
    let _ = send.write_all(&prologue_bytes).await;
    Ok(send)
}

/// Sends one `TransportFeedback` Envelope on an already-opened feedback
/// stream.
pub async fn send_transport_feedback(
    send: &mut quinn::SendStream,
    feedback: &TransportFeedback,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::TRANSPORT_FEEDBACK,
        &messages::encode(feedback),
    )
    .await
}

/// Server side: accepts the next incoming unidirectional stream and reads
/// back one `TransportFeedback` (test/verification helper; a real server
/// would keep reading in a loop for the session's duration).
pub async fn read_transport_feedback(
    connection: &quinn::Connection,
) -> Result<TransportFeedback, ReadFeedbackError> {
    let recv = connection
        .accept_uni()
        .await
        .map_err(ReadFeedbackError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    let stream_prologue = reader
        .read_prologue()
        .await
        .map_err(ReadFeedbackError::Read)?;
    if stream_prologue.kind != StreamKind::Feedback {
        return Err(ReadFeedbackError::WrongStreamKind);
    }

    let (type_raw, payload) = reader
        .read_envelope(StreamKind::Feedback.max_envelope_length())
        .await
        .map_err(ReadFeedbackError::Read)?;
    if type_raw != messages::type_id::TRANSPORT_FEEDBACK {
        return Err(ReadFeedbackError::UnexpectedType(type_raw));
    }
    messages::decode(&payload).map_err(ReadFeedbackError::Decode)
}

/// Per-frame client-local timestamps needed to build a `TransportFeedback`
/// for that frame (spec 2.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTimestamps {
    /// Client monotonic clock, when the frame's bytes finished arriving.
    pub receive_ts: u64,
    /// Client monotonic clock, when decoding finished.
    pub decode_done_ts: u64,
    /// Client monotonic clock, when the frame was submitted for display.
    pub display_ts: u64,
}

/// Builds a `TransportFeedback` for one received/decoded/displayed frame.
///
/// `offset_us` is the TimeSync `offset_us` (responder/server clock minus
/// requester/client clock, [`crate::timesync::compute`]), used to convert
/// `timestamps.display_ts` (client clock) into the server clock so it can
/// be compared against `header.capture_ts` (spec 2.10's
/// `client_queue_delay_us`, the backpressure primary signal).
///
/// `receive_bitrate_bps` here is a single-frame instantaneous estimate
/// (payload size over the nominal `TRANSPORT_FEEDBACK_INTERVAL`, spec
/// 4.7's 100ms) -- a real client would track this over the actual
/// inter-feedback interval instead.
pub fn build_transport_feedback(
    header: &VideoFrameHeader,
    payload_len: usize,
    timestamps: &FrameTimestamps,
    offset_us: i64,
    target_latency_us: u32,
) -> TransportFeedback {
    const TRANSPORT_FEEDBACK_INTERVAL_US: u64 = 100_000;

    let decode_delay_us = timestamps
        .decode_done_ts
        .saturating_sub(timestamps.receive_ts) as u32;
    let display_delay_us = timestamps
        .display_ts
        .saturating_sub(timestamps.decode_done_ts) as u32;

    let display_ts_server_clock = timesync::to_responder_clock(timestamps.display_ts, offset_us);
    let client_queue_delay_us = display_ts_server_clock.saturating_sub(header.capture_ts) as u32;

    let receive_bitrate_bps = (payload_len as u64 * 8 * 1_000_000) / TRANSPORT_FEEDBACK_INTERVAL_US;

    TransportFeedback {
        last_received_frame_id: header.frame_id,
        last_decoded_frame_id: header.frame_id,
        last_displayed_frame_id: header.frame_id,
        frames_received: 1,
        frames_dropped: 0,
        receive_bitrate_bps,
        decode_delay_us,
        display_delay_us,
        target_latency_us,
        client_queue_delay_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> VideoFrameHeader {
        VideoFrameHeader {
            generation: 0,
            frame_id: 7,
            config_id: 1,
            flags: 1,
            capture_ts: 1_000_000,
            encode_done_ts: 1_000_500,
            width: 640,
            height: 360,
            payload_len: 5_000,
        }
    }

    #[test]
    fn frame_ids_are_carried_through() {
        let ts = FrameTimestamps {
            receive_ts: 100,
            decode_done_ts: 120,
            display_ts: 130,
        };
        let fb = build_transport_feedback(&header(), 5_000, &ts, 0, 50_000);
        assert_eq!(fb.last_received_frame_id, 7);
        assert_eq!(fb.last_decoded_frame_id, 7);
        assert_eq!(fb.last_displayed_frame_id, 7);
        assert_eq!(fb.frames_received, 1);
        assert_eq!(fb.frames_dropped, 0);
    }

    #[test]
    fn decode_and_display_delays_are_measured_between_the_right_timestamps() {
        let ts = FrameTimestamps {
            receive_ts: 100,
            decode_done_ts: 150, // 50us to decode
            display_ts: 170,     // 20us to display after decode
        };
        let fb = build_transport_feedback(&header(), 5_000, &ts, 0, 50_000);
        assert_eq!(fb.decode_delay_us, 50);
        assert_eq!(fb.display_delay_us, 20);
    }

    #[test]
    fn client_queue_delay_uses_timesync_offset_to_reach_server_clock() {
        // capture_ts (server clock) = 1_000_000.
        // display_ts (client clock) = 100; offset = server - client, so
        // if the server is ahead by 999_920, converting display_ts to the
        // server clock lands exactly at 1_000_020 -> a 20us queue delay.
        let ts = FrameTimestamps {
            receive_ts: 50,
            decode_done_ts: 80,
            display_ts: 100,
        };
        let offset_us = 999_920;
        let fb = build_transport_feedback(&header(), 5_000, &ts, offset_us, 50_000);
        assert_eq!(fb.client_queue_delay_us, 20);
    }

    #[test]
    fn target_latency_is_passed_through_unmodified() {
        let ts = FrameTimestamps {
            receive_ts: 0,
            decode_done_ts: 0,
            display_ts: 0,
        };
        let fb = build_transport_feedback(&header(), 5_000, &ts, 1_000_000, 42_000);
        assert_eq!(fb.target_latency_us, 42_000);
    }

    #[test]
    fn receive_bitrate_is_computed_from_payload_size() {
        let ts = FrameTimestamps {
            receive_ts: 0,
            decode_done_ts: 0,
            display_ts: 0,
        };
        // 5000 bytes = 40000 bits, over a nominal 100ms interval ->
        // 400,000 bps.
        let fb = build_transport_feedback(&header(), 5_000, &ts, 0, 0);
        assert_eq!(fb.receive_bitrate_bps, 400_000);
    }
}
