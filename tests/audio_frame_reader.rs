//! Regression test: `AudioFrameReader::read_next_frame` must be
//! cancellation-safe, the same property `stage3w1d_video_reader.rs`
//! pins for `VideoFrameReader` (3W-1-d-3). `sardp-client` polls audio
//! playback inside the same `select!` as video frames and window timing
//! reports; if the future is dropped after the `AudioFrameHeader`
//! Envelope was consumed but before the `AudioFramePayload` arrived, the
//! next call must resume at the payload -- not misread the payload as a
//! header (`PROTOCOL_UNEXPECTED_MESSAGE`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use sardp::audio_session::{accept_audio_stream, open_audio_stream, send_audio_frame};
use sardp::messages::{self, AudioCodec, AudioConfig, AudioFrameHeader};
use sardp::stream_kind::StreamKind;
use sardp::stream_reader::write_envelope;
use sardp::{net, pki};

/// Same `SARDP_TEST_BIND_ADDR` override as `stage3w1d_input.rs`
/// (KNOWN_ISSUES.md item 14: UDP loopback broken on the dev machine).
fn loopback(port: u16) -> SocketAddr {
    let ip = std::env::var("SARDP_TEST_BIND_ADDR")
        .ok()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .unwrap_or(Ipv4Addr::LOCALHOST);
    SocketAddr::new(IpAddr::V4(ip), port)
}

async fn connect_pair() -> (quinn::Connection, quinn::Connection) {
    let test_cert = pki::generate_test_certificate("localhost");
    let server_endpoint = net::server_endpoint(loopback(0), &test_cert);
    let server_addr = server_endpoint.local_addr().unwrap();
    let client_endpoint = net::client_endpoint(loopback(0), &test_cert.cert_der);

    let server_accept = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.expect("incoming connection");
        let connection = incoming.await.expect("server-side handshake");
        (server_endpoint, connection)
    });
    let client_connection = client_endpoint
        .connect(server_addr, "localhost")
        .expect("valid connect params")
        .await
        .expect("client-side handshake");
    let (_server_endpoint, server_connection) = server_accept.await.unwrap();
    (client_connection, server_connection)
}

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
