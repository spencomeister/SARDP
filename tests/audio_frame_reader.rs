//! Regression test: `AudioFrameReader::read_next_frame` must be
//! cancellation-safe, the same property `stage3w1d_video_reader.rs`
//! pins for `VideoFrameReader` (3W-1-d-3). `sardp-client` polls audio
//! playback inside the same `select!` as video frames and window timing
//! reports; if the future is dropped after the `AudioFrameHeader`
//! Envelope was consumed but before the `AudioFramePayload` arrived, the
//! next call must resume at the payload -- not misread the payload as a
//! header (`PROTOCOL_UNEXPECTED_MESSAGE`).

mod common;

use common::connect_pair;

use std::time::Duration;

use sardp::audio_session::{accept_audio_stream, open_audio_stream, send_audio_frame};
use sardp::messages::{self, AudioCodec, AudioConfig, AudioFrameHeader};
use sardp::stream_kind::StreamKind;
use sardp::stream_reader::write_envelope;

#[tokio::test]
async fn read_next_frame_survives_cancellation_between_header_and_payload() {
    let (client_connection, server_connection) = connect_pair().await;

    let config = AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48_000,
        channels: 1,
        frame_duration_ms: 20,
    };
    // audio_playback: server opens, client accepts (the direction
    // sardp-client reads inside its select!).
    let (server_result, client_result) = tokio::join!(
        open_audio_stream(&server_connection, StreamKind::AudioPlayback, &config),
        accept_audio_stream(&client_connection, StreamKind::AudioPlayback),
    );
    let mut send_stream = server_result.expect("server opens the audio stream");
    let (received_config, mut frame_reader) = client_result.expect("client accepts the stream");
    assert_eq!(received_config, config);

    // Frame 1: header only, for now.
    let payload_1 = vec![0x11, 0x22, 0x33, 0x44];
    let header_1 = AudioFrameHeader {
        sequence: 1,
        capture_ts: 100,
        duration_us: 20_000,
        payload_len: payload_1.len() as u64,
    };
    write_envelope(
        &mut send_stream,
        messages::type_id::AUDIO_FRAME_HEADER,
        &messages::encode(&header_1),
    )
    .await
    .expect("header write");

    // The read consumes the header and then waits for the payload; the
    // timeout drops (cancels) it in that state.
    let cancelled =
        tokio::time::timeout(Duration::from_millis(300), frame_reader.read_next_frame()).await;
    assert!(
        cancelled.is_err(),
        "the read must still be waiting for the payload"
    );

    // Now the payload, and a complete frame 2 behind it.
    write_envelope(
        &mut send_stream,
        messages::type_id::AUDIO_FRAME_PAYLOAD,
        &payload_1,
    )
    .await
    .expect("payload write");
    let payload_2 = vec![0x55, 0x66];
    send_audio_frame(&mut send_stream, 2, 200, 20_000, &payload_2)
        .await
        .expect("frame 2 write");

    let (header, payload) =
        tokio::time::timeout(Duration::from_secs(5), frame_reader.read_next_frame())
            .await
            .expect("frame 1 arrives")
            .expect("frame 1 reads cleanly after the cancelled attempt");
    assert_eq!(header, header_1);
    assert_eq!(payload, payload_1);

    let (header, payload) =
        tokio::time::timeout(Duration::from_secs(5), frame_reader.read_next_frame())
            .await
            .expect("frame 2 arrives")
            .expect("frame 2 reads cleanly");
    assert_eq!(header.sequence, 2);
    assert_eq!(payload, payload_2);
}
