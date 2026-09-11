//! Stage 3 (3W-1-d-3) regression test: `VideoFrameReader::read_next_frame`
//! must be cancellation-safe. `sardp-client` polls it inside a `select!`
//! alongside arms that fire between frames (window decode/present timing
//! reports, control messages, audio); if the future is dropped after the
//! `VideoFrameHeader` Envelope was consumed but before the
//! `VideoFramePayload` arrived, the next call must resume at the payload
//! -- not misread the payload as a header (`PROTOCOL_UNEXPECTED_MESSAGE`,
//! which is exactly what the first d-3 run died of).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use sardp::messages::{self, ChromaFormat, Codec, EncoderConfig, VideoFrameHeader};
use sardp::stream_reader::write_envelope;
use sardp::video_session::{accept_video_instance, open_video_instance};
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

    let encoder_config = EncoderConfig {
        codec: Codec::H264,
        profile: 66,
        chroma_format: ChromaFormat::C420,
        bit_depth: 8,
        width: 64,
        height: 64,
        max_fps: 30,
        tier: 4,
        b_frames: 0,
        server_cursor_excludable: false,
    };
    let fake_idr_payload = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB];
    let (server_result, client_result) = tokio::join!(
        open_video_instance(&server_connection, 0, 0, 1, encoder_config, fake_idr_payload, 0, 0),
        accept_video_instance(&client_connection),
    );
    let (mut send_stream, _sm) = server_result.expect("server sends the instance intro");
    let (_intro, mut frame_reader) = client_result.expect("client accepts the instance");

    // Frame 1: header only, for now.
    let payload_1 = vec![0x00, 0x00, 0x00, 0x01, 0x61, 0x11, 0x22, 0x33];
    let header_1 = VideoFrameHeader {
        generation: 0,
        frame_id: 1,
        config_id: 1,
        flags: 0,
        capture_ts: 100,
        encode_done_ts: 150,
        width: 64,
        height: 64,
        payload_len: payload_1.len() as u64,
    };
    write_envelope(
        &mut send_stream,
        messages::type_id::VIDEO_FRAME_HEADER,
        &messages::encode(&header_1),
    )
    .await
    .expect("header write");

    // The read consumes the header and then waits for the payload; the
    // timeout drops (cancels) it in that state.
    let cancelled = tokio::time::timeout(Duration::from_millis(300), frame_reader.read_next_frame()).await;
    assert!(cancelled.is_err(), "the read must still be waiting for the payload");

    // Now the payload, and a complete frame 2 behind it.
    write_envelope(&mut send_stream, messages::type_id::VIDEO_FRAME_PAYLOAD, &payload_1)
        .await
        .expect("payload write");
    let payload_2 = vec![0x00, 0x00, 0x00, 0x01, 0x61, 0x44];
    sardp::video_session::send_video_frame(&mut send_stream, 0, 2, 1, 0, 200, 250, 64, 64, &payload_2)
        .await
        .expect("frame 2 write");

    let (header, payload) = tokio::time::timeout(Duration::from_secs(5), frame_reader.read_next_frame())
        .await
        .expect("frame 1 arrives")
        .expect("frame 1 reads cleanly after the cancelled attempt");
    assert_eq!(header, header_1);
    assert_eq!(payload, payload_1);

    let (header, payload) = tokio::time::timeout(Duration::from_secs(5), frame_reader.read_next_frame())
        .await
        .expect("frame 2 arrives")
        .expect("frame 2 reads cleanly");
    assert_eq!(header.frame_id, 2);
    assert_eq!(payload, payload_2);
}
