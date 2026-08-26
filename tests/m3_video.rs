//! M3 integration test: single video stream Instance opening over a real
//! loopback QUIC connection. Generates a synthetic timecode frame,
//! encodes it via ffmpeg into a self-contained H.264 IDR, and sends
//! StreamPrologue -> VideoStreamGeneration -> EncoderConfig ->
//! VideoFrameHeader -> VideoFramePayload (spec 2.10, 2.2.1, 4.3.2,
//! DR-035) to a receiver that validates the wire contract (message
//! order, header/payload length consistency, IDR self-containment).
//! Client-side decode/display is M4's scope, not tested here.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sardp::encoder::{encode_single_frame_idr, ffmpeg_available};
use sardp::h264::is_self_contained_idr;
use sardp::messages::{
    self, ChromaFormat, Codec, EncoderConfig, VideoFrameHeader, VideoStreamGeneration,
};
use sardp::stream_kind::StreamKind;
use sardp::timecode_frame::generate_timecode_frame;
use sardp::video_session::{
    VideoError, VideoInstanceIntro, open_video_instance, read_video_instance_intro,
};
use sardp::video_sm::InstanceState;
use sardp::{net, pki, prologue};

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

fn skip_if_no_ffmpeg() -> bool {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not found on PATH");
        return true;
    }
    false
}

#[tokio::test]
async fn opens_video_instance_and_delivers_self_contained_idr() {
    if skip_if_no_ffmpeg() {
        return;
    }
    let (client_connection, server_connection) = connect_pair().await;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;
    const MONITOR_ID: u64 = 0;
    const GENERATION: u64 = 0;
    const CONFIG_ID: u64 = 1;
    let capture_ts: u64 = 1_000_000;
    let encode_done_ts: u64 = 1_000_800;

    let frame = generate_timecode_frame(WIDTH, HEIGHT, capture_ts, [40, 40, 40]);
    let h264_bytes = encode_single_frame_idr(&frame).expect("ffmpeg encode succeeds");
    assert!(
        is_self_contained_idr(&h264_bytes),
        "encoder must produce SPS+PPS before the IDR slice"
    );

    let encoder_config = EncoderConfig {
        codec: Codec::H264,
        profile: 66, // baseline
        chroma_format: ChromaFormat::C420,
        bit_depth: 8,
        width: WIDTH,
        height: HEIGHT,
        max_fps: 30,
        tier: 4, // software encode, spec 2.10/Media Model Tier table
        b_frames: 0,
        server_cursor_excludable: false,
    };

    let (server_result, client_result) = tokio::join!(
        open_video_instance(
            &server_connection,
            MONITOR_ID,
            GENERATION,
            CONFIG_ID,
            encoder_config,
            h264_bytes.clone(),
            capture_ts,
            encode_done_ts,
        ),
        read_video_instance_intro(&client_connection),
    );

    let (_send_stream, server_sm) = server_result.expect("server sends the instance intro");
    let intro: VideoInstanceIntro = client_result.expect("client receives the instance intro");

    assert_eq!(server_sm.state(), InstanceState::Streaming);

    assert_eq!(intro.monitor_id, MONITOR_ID);
    assert_eq!(intro.generation.generation, GENERATION);
    assert_eq!(intro.generation.config_id, CONFIG_ID);

    assert_eq!(intro.encoder_config.codec, Codec::H264);
    assert_eq!(
        intro.encoder_config.b_frames, 0,
        "B-frames MUST be 0 (DR-019)"
    );
    assert_eq!(intro.encoder_config.width, WIDTH);
    assert_eq!(intro.encoder_config.height, HEIGHT);

    assert_eq!(intro.first_frame_header.generation, GENERATION);
    assert_eq!(intro.first_frame_header.frame_id, 0);
    assert!(intro.first_frame_header.is_idr());
    assert_eq!(
        intro.first_frame_header.payload_len,
        intro.first_frame_payload.len() as u64
    );
    assert_eq!(intro.first_frame_header.capture_ts, capture_ts);
    assert_eq!(intro.first_frame_header.encode_done_ts, encode_done_ts);
    assert_eq!(intro.first_frame_payload, h264_bytes);
    assert!(
        is_self_contained_idr(&intro.first_frame_payload),
        "the frame as received over the wire must still be self-contained"
    );
}

#[tokio::test]
async fn frame_ids_and_generation_are_monotonic_from_zero() {
    // Wire-level sanity check independent of ffmpeg: the first Instance
    // of a fresh Channel always starts at generation 0, frame_id 0 (spec
    // 2.10). Uses a fake (non-ffmpeg) payload so it always runs.
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
        open_video_instance(
            &server_connection,
            0,
            0,
            1,
            encoder_config,
            fake_idr_payload,
            0,
            0,
        ),
        read_video_instance_intro(&client_connection),
    );
    server_result.expect("server sends the instance intro");
    let intro = client_result.expect("client receives the instance intro");
    assert_eq!(intro.generation.generation, 0);
    assert_eq!(intro.first_frame_header.frame_id, 0);
}

#[tokio::test]
async fn payload_not_immediately_after_header_is_rejected() {
    // Spec 2.10 / DR-035: VideoFramePayload MUST immediately follow
    // VideoFrameHeader with nothing else in between. Hand-craft a
    // malformed stream (header, then some other Envelope instead of the
    // payload) to prove the receiver actually enforces this rather than
    // just happening to work when a well-behaved sender is on the other
    // end.
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

    // Not `async move`: `server_connection` must stay alive in this
    // function's scope for the whole `join!` (see the M2 handshake test's
    // note on why `tokio::spawn`/moving a `Connection` into a short-lived
    // task causes an implicit `ApplicationClose` that can race the read).
    let send_malformed = async {
        let mut send = server_connection.open_uni().await.unwrap();

        let mut buf = Vec::new();
        prologue::encode(StreamKind::Video, 1, 0, &mut buf);
        send.write_all(&buf).await.unwrap();

        buf.clear();
        sardp::envelope::encode(
            messages::type_id::VIDEO_STREAM_GENERATION,
            &messages::encode(&VideoStreamGeneration {
                generation: 0,
                config_id: 1,
            }),
            &mut buf,
        );
        send.write_all(&buf).await.unwrap();

        buf.clear();
        sardp::envelope::encode(
            messages::type_id::ENCODER_CONFIG,
            &messages::encode(&encoder_config),
            &mut buf,
        );
        send.write_all(&buf).await.unwrap();

        buf.clear();
        sardp::envelope::encode(
            messages::type_id::VIDEO_FRAME_HEADER,
            &messages::encode(&VideoFrameHeader {
                generation: 0,
                frame_id: 0,
                config_id: 1,
                flags: 1,
                capture_ts: 0,
                encode_done_ts: 0,
                width: 64,
                height: 64,
                payload_len: 3,
            }),
            &mut buf,
        );
        send.write_all(&buf).await.unwrap();

        // Instead of VideoFramePayload, send another VideoStreamGeneration.
        buf.clear();
        sardp::envelope::encode(
            messages::type_id::VIDEO_STREAM_GENERATION,
            &messages::encode(&VideoStreamGeneration {
                generation: 1,
                config_id: 1,
            }),
            &mut buf,
        );
        send.write_all(&buf).await.unwrap();
    };

    let (_, client_result) = tokio::join!(
        send_malformed,
        read_video_instance_intro(&client_connection)
    );

    assert!(matches!(
        client_result,
        Err(VideoError::ProtocolViolation(_))
    ));
}
