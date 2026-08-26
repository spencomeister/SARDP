//! M4 integration test, extending the M2 (handshake) and M3 (video)
//! harnesses over one real loopback QUIC connection:
//!
//! 1. Handshake to `Authenticated` (M2).
//! 2. TimeSync exchange over the still-open `control` stream (spec 2.9).
//! 3. Open + receive a video Instance (M3, DR-035's two-Envelope frame).
//! 4. Client decodes the frame (ffmpeg) and submits it to `ClientDisplay`
//!    (spec 2.10's client-side display rules).
//! 5. Both sides drive their `ChannelSm` to `Live` and their
//!    `ConnectionSm` from `Authenticated` to `Active` (DR-024).
//! 6. Client builds and sends a `TransportFeedback` using the TimeSync
//!    offset; server receives and decodes it (spec 2.14).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ed25519_dalek::SigningKey;
use sardp::channel_sm::{ChannelSm, ChannelState};
use sardp::client_display::{ClientDisplay, SubmitOutcome};
use sardp::connection_sm::ConnectionState;
use sardp::decoder::decode_single_frame;
use sardp::encoder::{encode_single_frame_idr, ffmpeg_available};
use sardp::feedback_session::{
    FrameTimestamps, build_transport_feedback, open_feedback_stream, read_transport_feedback,
    send_transport_feedback,
};
use sardp::handshake::{client_handshake, server_handshake};
use sardp::messages::{ChromaFormat, Codec, EncoderConfig};
use sardp::timecode_frame::generate_timecode_frame;
use sardp::timesync::{client_time_sync, server_respond_time_sync};
use sardp::video_session::{open_video_instance, read_video_instance_intro};
use sardp::{clock, net, pki};

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
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
async fn full_session_reaches_active_with_feedback_round_trip() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not found on PATH");
        return;
    }
    let (client_connection, server_connection) = connect_pair().await;

    // 1. Handshake (M2). `join!` (not sequential `.await`s): client and
    // server each block on messages only the other side sends, so both
    // must be driven concurrently.
    let client_signing_key = SigningKey::from_bytes(&[0x55; 32]);
    let trusted_public_key = client_signing_key.verifying_key();
    let (client_handshake_result, server_handshake_result) = tokio::join!(
        client_handshake(
            &client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        server_handshake(&server_connection, "test-server", &trusted_public_key),
    );
    let (_client_outcome, mut client_sm, mut client_control) =
        client_handshake_result.expect("client handshake succeeds");
    let (_server_outcome, mut server_sm, mut server_control) =
        server_handshake_result.expect("server handshake succeeds");

    // 2. TimeSync (spec 2.9), over the still-open control stream.
    let (client_timesync_result, server_timesync_result) = tokio::join!(
        client_time_sync(&mut client_control),
        server_respond_time_sync(&mut server_control),
    );
    let timesync_result = client_timesync_result.expect("client TimeSync succeeds");
    server_timesync_result.expect("server TimeSync succeeds");

    // 3. Open + receive one video Instance (M3).
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;
    let capture_ts = clock::now_us();
    let frame = generate_timecode_frame(WIDTH, HEIGHT, capture_ts, [40, 40, 40]);
    let h264_bytes = encode_single_frame_idr(&frame).expect("ffmpeg encode succeeds");
    let encoder_config = EncoderConfig {
        codec: Codec::H264,
        profile: 66,
        chroma_format: ChromaFormat::C420,
        bit_depth: 8,
        width: WIDTH,
        height: HEIGHT,
        max_fps: 30,
        tier: 4,
        b_frames: 0,
        server_cursor_excludable: false,
    };
    let encode_done_ts = clock::now_us();

    let (server_video_result, client_video_result) = tokio::join!(
        open_video_instance(
            &server_connection,
            0,
            0,
            1,
            encoder_config,
            h264_bytes,
            capture_ts,
            encode_done_ts,
        ),
        read_video_instance_intro(&client_connection),
    );
    let (_send_stream, video_instance_sm) =
        server_video_result.expect("server opens the video instance");
    let intro = client_video_result.expect("client receives the video instance");

    // 4. Client decodes and displays (M4's core new behavior).
    let receive_ts = clock::now_us();
    let decoded = decode_single_frame(&intro.first_frame_payload, WIDTH, HEIGHT)
        .expect("ffmpeg decode succeeds");
    let decode_done_ts = clock::now_us();

    let mut client_display = ClientDisplay::new();
    let submit_outcome = client_display.submit_frame(&intro.first_frame_header, decoded);
    let display_ts = clock::now_us();
    assert_eq!(submit_outcome, SubmitOutcome::Displayed);
    assert_eq!(client_display.current_generation(), Some(0));
    assert!(client_display.displayed_frame().is_some());

    // 5. Channel -> Live, Connection -> Active (DR-024), on both sides.
    assert_eq!(
        video_instance_sm.state(),
        sardp::video_sm::InstanceState::Streaming
    );
    let mut server_channel = ChannelSm::new();
    server_channel.on_instance_streaming();
    assert_eq!(server_channel.state(), ChannelState::Live);
    server_sm
        .on_channel_live()
        .expect("Authenticated -> Active on the server");
    assert_eq!(server_sm.state(), ConnectionState::Active);

    let mut client_channel = ChannelSm::new();
    client_channel.on_instance_streaming();
    assert_eq!(client_channel.state(), ChannelState::Live);
    client_sm
        .on_channel_live()
        .expect("Authenticated -> Active on the client");
    assert_eq!(client_sm.state(), ConnectionState::Active);

    // 6. Client sends TransportFeedback (spec 2.14) using the TimeSync
    // offset; server receives it.
    let feedback = build_transport_feedback(
        &intro.first_frame_header,
        intro.first_frame_payload.len(),
        &FrameTimestamps {
            receive_ts,
            decode_done_ts,
            display_ts,
        },
        timesync_result.offset_us,
        50_000,
    );

    let mut feedback_send = open_feedback_stream(&client_connection)
        .await
        .expect("client opens the feedback stream");
    let (send_result, received_feedback) = tokio::join!(
        send_transport_feedback(&mut feedback_send, &feedback),
        read_transport_feedback(&server_connection),
    );
    send_result.expect("feedback send succeeds");
    let received_feedback = received_feedback.expect("server receives feedback");

    assert_eq!(received_feedback, feedback);
    assert_eq!(received_feedback.last_received_frame_id, 0);
    assert_eq!(received_feedback.frames_received, 1);
    assert_eq!(received_feedback.frames_dropped, 0);
}
