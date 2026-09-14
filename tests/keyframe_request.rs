//! `KeyframeRequest` (spec 2.10) on the `feedback` stream over real
//! loopback QUIC.
//!
//! - Wire level: the client's queue circuit breaker
//!   (`sardp::queue_circuit_breaker`) sends it on the same stream that
//!   carries `TransportFeedback`, and a server reading that stream with
//!   `FeedbackReceiver::read_message` must see both, in order, without
//!   tripping `UnexpectedType`.
//! - End to end: client queue over the threshold -> `KeyframeRequest`
//!   -> server honors it (`VideoChannel::on_keyframe_request`) and
//!   reopens at `generation + 1` with a self-contained IDR -> the client
//!   accepts the new generation, its backlog drains, the breaker re-arms
//!   -- and a second request inside the server's minimum interval is
//!   ignored. The server side is driven the way `sardp-server`'s session
//!   loop does it (same calls, same order), with the same fake IDR
//!   payloads `m5_backpressure.rs` uses: this is about the protocol
//!   mechanism, not video content, so it runs without `ffmpeg`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sardp::channel_sm::ChannelState;
use sardp::client_display::{ClientDisplay, SubmitOutcome};
use sardp::feedback_session::{
    FeedbackMessage, FeedbackReceiver, open_feedback_stream, send_keyframe_request,
    send_transport_feedback,
};
use sardp::messages::{
    ChromaFormat, Codec, EncoderConfig, KeyframeReason, KeyframeRequest, TransportFeedback,
};
use sardp::queue_circuit_breaker::{ClientQueueCircuitBreaker, defaults};
use sardp::video_channel::{
    KeyframeRequestDecision, KeyframeRequestIgnored, VideoChannel,
    defaults::KEYFRAME_REQUEST_MIN_INTERVAL_US,
};
use sardp::video_session::{open_video_instance, read_video_instance_intro};
use sardp::video_sm::InstanceState;
use sardp::{net, pki};

/// Local bind address for the test endpoints. Defaults to 127.0.0.1, but
/// honors `SARDP_TEST_BIND_ADDR` (an IPv4 address) so the test can still
/// run on a machine whose UDP *loopback* is broken while UDP over a real
/// local interface works (KNOWN_ISSUES.md item 14 -- e.g. set it to the
/// machine's LAN address). Same override as `stage3w1d_input.rs`.
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

fn test_encoder_config() -> EncoderConfig {
    EncoderConfig {
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
    }
}

fn fake_idr(tag: u8) -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 0x67, tag, 0xBB]
}

fn transport_feedback(client_queue_delay_us: u32) -> TransportFeedback {
    TransportFeedback {
        last_received_frame_id: 10,
        last_decoded_frame_id: 10,
        last_displayed_frame_id: 10,
        frames_received: 1,
        frames_dropped: 0,
        receive_bitrate_bps: 1_000_000,
        decode_delay_us: 500,
        display_delay_us: 100,
        target_latency_us: 50_000,
        client_queue_delay_us,
    }
}

#[tokio::test]
async fn keyframe_request_travels_on_the_feedback_stream_next_to_transport_feedback() {
    let (client, server) = connect_pair().await;

    let client_task = tokio::spawn(async move {
        let mut send = open_feedback_stream(&client)
            .await
            .expect("client opens feedback stream");
        // Normal feedback first, exactly as a client does every 100ms...
        send_transport_feedback(&mut send, &transport_feedback(25))
            .await
            .expect("TransportFeedback sent");
        // ...then the breaker trips on a queue that has grown past the
        // threshold and the client sends what it hands back.
        let mut breaker = ClientQueueCircuitBreaker::new();
        let request = breaker
            .observe(defaults::TRIP_THRESHOLD_US + 1)
            .expect("breaker trips");
        send_keyframe_request(&mut send, &request)
            .await
            .expect("KeyframeRequest sent");
        // Still above the threshold: nothing more to send.
        assert_eq!(breaker.observe(defaults::TRIP_THRESHOLD_US * 2), None);
        // Feedback keeps flowing on the same stream afterwards.
        send_transport_feedback(&mut send, &transport_feedback(1_200_000))
            .await
            .expect("TransportFeedback after the request sent");
        send.finish().expect("finish");
        client
    });

    let mut receiver = FeedbackReceiver::accept(&server)
        .await
        .expect("server accepts feedback stream");
    assert_eq!(
        receiver.read_message().await.expect("first message"),
        FeedbackMessage::Transport(transport_feedback(25))
    );
    assert_eq!(
        receiver.read_message().await.expect("second message"),
        FeedbackMessage::Keyframe(KeyframeRequest {
            reason: KeyframeReason::DecodeError,
        })
    );
    assert_eq!(
        receiver.read_message().await.expect("third message"),
        FeedbackMessage::Transport(transport_feedback(1_200_000))
    );

    let _client = client_task.await.unwrap();
}

#[tokio::test]
async fn client_breaker_trip_makes_the_server_reopen_at_the_next_generation() {
    let (client_connection, server_connection) = connect_pair().await;

    // --- Generation 0 is up (M3 mechanics), server-side bookkeeping as
    // in sardp-server's session loop.
    let (server_send_0, client_intro_0) = tokio::join!(
        open_video_instance(
            &server_connection,
            0,
            0,
            1,
            test_encoder_config(),
            fake_idr(0xAA),
            1_000_000,
            1_000_100,
        ),
        read_video_instance_intro(&client_connection),
    );
    let (mut server_video_send, _sm_0) = server_send_0.expect("server opens generation 0");
    let intro_0 = client_intro_0.expect("client receives generation 0");
    assert_eq!(intro_0.generation.generation, 0);

    let mut channel = VideoChannel::new(0);
    channel.mark_instance_streaming().unwrap();
    assert_eq!(channel.channel_state(), ChannelState::Live);

    let mut feedback_send = open_feedback_stream(&client_connection)
        .await
        .expect("client opens feedback stream");
    let mut feedback_recv = FeedbackReceiver::accept(&server_connection)
        .await
        .expect("server accepts feedback stream");

    // --- Client: generation 0's first frame is on screen, the queue is
    // healthy, and normal feedback flows. The server sees nothing that
    // its own backpressure would act on.
    let mut display = ClientDisplay::<()>::new();
    assert_eq!(
        display.submit_frame(&intro_0.first_frame_header, ()),
        SubmitOutcome::Displayed
    );
    let mut breaker = ClientQueueCircuitBreaker::new();
    assert_eq!(breaker.observe(25), None);
    let healthy = transport_feedback(25);
    let (sent, received) = tokio::join!(
        send_transport_feedback(&mut feedback_send, &healthy),
        feedback_recv.read_message(),
    );
    sent.expect("feedback sent");
    let FeedbackMessage::Transport(feedback) = received.expect("feedback read") else {
        panic!("expected TransportFeedback");
    };
    let server_now_us = 20_000_000u64;
    channel
        .on_feedback(server_now_us, feedback.client_queue_delay_us, 0)
        .unwrap();
    assert_eq!(channel.instance_state(), InstanceState::Streaming);

    // --- Client: the decoder stalls; the oldest queued frame ages past
    // the trip threshold and the breaker hands back one request.
    let request = breaker
        .observe(defaults::TRIP_THRESHOLD_US + 1)
        .expect("breaker trips");
    assert_eq!(request.reason, KeyframeReason::DecodeError);
    let (sent, received) = tokio::join!(
        send_keyframe_request(&mut feedback_send, &request),
        feedback_recv.read_message(),
    );
    sent.expect("KeyframeRequest sent");
    let FeedbackMessage::Keyframe(received_request) = received.expect("request read") else {
        panic!("expected KeyframeRequest");
    };
    assert_eq!(received_request, request);

    // --- Server: honor it, exactly as after a backpressure ResetStream:
    // RESET_STREAM, generation+1, fresh self-contained IDR.
    let server_now_us = server_now_us + 1_100_000;
    assert_eq!(
        channel.on_keyframe_request(server_now_us, received_request),
        KeyframeRequestDecision::Reopen
    );
    assert_eq!(channel.channel_state(), ChannelState::Recovering);
    server_video_send
        .reset(quinn::VarInt::from_u32(0))
        .expect("resetting generation 0's stream");
    let new_generation = channel.prepare_reopen();
    assert_eq!(new_generation, 1);
    let (server_send_1, client_intro_1) = tokio::join!(
        open_video_instance(
            &server_connection,
            0,
            new_generation,
            1,
            test_encoder_config(),
            fake_idr(0xBB),
            2_000_000,
            2_000_100,
        ),
        read_video_instance_intro(&client_connection),
    );
    let (_server_video_send_1, _sm_1) = server_send_1.expect("server reopens at generation 1");
    channel.mark_instance_streaming().unwrap();
    assert_eq!(channel.channel_state(), ChannelState::Live);
    assert_eq!(channel.generation(), 1);

    // --- Client: accepts the new generation (spec 2.10 client MUST:
    // the old backlog is discarded, the new IDR is displayed), the queue
    // is empty again, and the breaker re-arms.
    let intro_1 = client_intro_1.expect("client receives generation 1");
    assert_eq!(intro_1.generation.generation, 1);
    assert_eq!(intro_1.first_frame_header.frame_id, 0);
    assert!(intro_1.first_frame_header.is_idr());
    assert_eq!(
        display.submit_frame(&intro_1.first_frame_header, ()),
        SubmitOutcome::Displayed
    );
    assert_eq!(display.current_generation(), Some(1));
    assert!(!display.would_display(&intro_0.first_frame_header));
    assert_eq!(breaker.observe(0), None);
    assert!(!breaker.is_tripped());

    // --- A second request arriving inside the server's minimum
    // interval (a duplicate, or one that crossed the reopen in flight)
    // is read fine but ignored: generation 1 stays up.
    let (sent, received) = tokio::join!(
        send_keyframe_request(&mut feedback_send, &request),
        feedback_recv.read_message(),
    );
    sent.expect("second KeyframeRequest sent");
    let FeedbackMessage::Keyframe(second) = received.expect("second request read") else {
        panic!("expected KeyframeRequest");
    };
    let too_soon_us = server_now_us + KEYFRAME_REQUEST_MIN_INTERVAL_US - 1;
    assert_eq!(
        channel.on_keyframe_request(too_soon_us, second),
        KeyframeRequestDecision::Ignored(KeyframeRequestIgnored::RateLimited {
            next_allowed_at_us: server_now_us + KEYFRAME_REQUEST_MIN_INTERVAL_US,
        })
    );
    assert_eq!(channel.instance_state(), InstanceState::Streaming);
    assert_eq!(channel.channel_state(), ChannelState::Live);
    assert_eq!(channel.generation(), 1);
}
