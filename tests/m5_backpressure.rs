//! M5 integration test: the PoC's core mechanism (spec 2.10, 4.3.1,
//! 4.3.2, DR-029), exercised over real loopback QUIC.
//!
//! Both tests drive `VideoChannel::on_feedback` with a `now_us` the test
//! controls explicitly (not real elapsed wall-clock time) so the 500ms
//! hysteresis and 10s baseline window can be exercised without the test
//! actually taking 10+ seconds -- only the *simulated* feedback-interval
//! timeline needs to span that long, not the real one. The
//! `TransportFeedback` messages themselves are real Envelopes sent over a
//! real QUIC stream and read back on the other end; only the "when did
//! this arrive" clock is simulated.
//!
//! Uses fake (non-ffmpeg) IDR payloads throughout, like
//! `m3_video.rs`'s `frame_ids_and_generation_are_monotonic_from_zero`:
//! backpressure is about the protocol mechanism, not real video content,
//! so these tests always run regardless of whether `ffmpeg` is
//! installed.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sardp::backpressure::BackpressureDecision;
use sardp::channel_sm::ChannelState;
use sardp::client_display::{ClientDisplay, SubmitOutcome};
use sardp::feedback_session::{FeedbackReceiver, open_feedback_stream, send_transport_feedback};
use sardp::messages::{ChromaFormat, Codec, EncoderConfig, TransportFeedback};
use sardp::video_channel::VideoChannel;
use sardp::video_session::{open_video_instance, read_video_instance_intro};
use sardp::video_sm::InstanceState;
use sardp::{net, pki};

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

/// Sends one synthetic `TransportFeedback` (only `client_queue_delay_us`
/// matters for this test; the rest are placeholders) and feeds it into
/// `channel` at logical time `now_us`, returning the decision.
async fn feed_sample(
    feedback_send: &mut quinn::SendStream,
    feedback_recv: &mut FeedbackReceiver,
    channel: &mut VideoChannel,
    now_us: u64,
    client_queue_delay_us: u32,
) -> BackpressureDecision {
    let feedback = TransportFeedback {
        last_received_frame_id: 0,
        last_decoded_frame_id: 0,
        last_displayed_frame_id: 0,
        frames_received: 1,
        frames_dropped: 0,
        receive_bitrate_bps: 0,
        decode_delay_us: 0,
        display_delay_us: 0,
        target_latency_us: 50_000,
        client_queue_delay_us,
    };
    let (send_result, received) = tokio::join!(
        send_transport_feedback(feedback_send, &feedback),
        feedback_recv.read_one()
    );
    send_result.expect("feedback send succeeds");
    let received = received.expect("feedback receive succeeds");
    assert_eq!(received.client_queue_delay_us, client_queue_delay_us);
    channel
        .on_feedback(now_us, received.client_queue_delay_us, 0)
        .expect("state transition is legal")
}

#[tokio::test]
async fn induced_congestion_resets_the_stream_and_recovers_on_a_new_generation() {
    let (client_connection, server_connection) = connect_pair().await;

    // Open + receive generation 0 (M3 mechanics).
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
    let (mut send_stream_0, _instance_sm_0) = server_send_0.expect("server opens generation 0");
    let intro_0 = client_intro_0.expect("client receives generation 0");
    assert_eq!(intro_0.generation.generation, 0);
    assert_eq!(intro_0.first_frame_header.frame_id, 0);

    let mut channel = VideoChannel::new(0);
    channel.mark_instance_streaming().unwrap();
    assert_eq!(channel.channel_state(), ChannelState::Live);

    // Set up the feedback stream (client sends, server reads).
    let mut feedback_send = open_feedback_stream(&client_connection)
        .await
        .expect("client opens feedback stream");
    let mut feedback_recv = FeedbackReceiver::accept(&server_connection)
        .await
        .expect("server accepts feedback stream");

    // Establish a healthy low baseline first, then induce congestion: a
    // sustained high client_queue_delay_us, as `tc netem`-style induced
    // congestion (bandwidth cap + buffer bloat, per the brief's
    // acceptance criteria) would actually produce.
    let mut now_us = 0u64;
    let d = feed_sample(
        &mut feedback_send,
        &mut feedback_recv,
        &mut channel,
        now_us,
        5_000,
    )
    .await;
    assert_eq!(d, BackpressureDecision::Continue);

    now_us += 100_000;
    let d = feed_sample(
        &mut feedback_send,
        &mut feedback_recv,
        &mut channel,
        now_us,
        200_000,
    )
    .await;
    assert_eq!(d, BackpressureDecision::EnterCongested);
    assert_eq!(channel.instance_state(), InstanceState::Congested);

    // Keep it congested for 3 consecutive intervals past the hard
    // threshold -> reset.
    let mut last_decision = d;
    for _ in 0..3 {
        now_us += 100_000;
        last_decision = feed_sample(
            &mut feedback_send,
            &mut feedback_recv,
            &mut channel,
            now_us,
            400_000,
        )
        .await;
    }
    assert_eq!(last_decision, BackpressureDecision::ResetStream);
    assert_eq!(channel.channel_state(), ChannelState::Recovering);

    // Act on the decision for real: RESET_STREAM the old Instance's QUIC
    // stream, bump the generation, and open a fresh self-contained IDR.
    send_stream_0
        .reset(quinn::VarInt::from_u32(0))
        .expect("resetting the old video stream succeeds");
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
    let (_send_stream_1, _instance_sm_1) = server_send_1.expect("server reopens at generation 1");
    let intro_1 = client_intro_1.expect("client receives generation 1");
    assert_eq!(intro_1.generation.generation, 1);
    assert_eq!(intro_1.first_frame_header.generation, 1);
    assert_eq!(intro_1.first_frame_header.frame_id, 0);
    // The IDR flag is this implementation's own protocol bookkeeping
    // (set by `open_video_instance`), independent of payload content --
    // meaningful even for this test's fake (non-ffmpeg) NAL bytes, unlike
    // real NAL-type parsing (see `m3_video.rs`/`encoder.rs` for that,
    // exercised against real ffmpeg output).
    assert!(intro_1.first_frame_header.is_idr());

    channel.mark_instance_streaming().unwrap();
    assert_eq!(channel.channel_state(), ChannelState::Live);
    assert_eq!(channel.instance_state(), InstanceState::Streaming);

    // Client-side consequence (spec 2.10 MUST): the new generation
    // supersedes the old one in the display buffer.
    let mut display = ClientDisplay::new();
    let outcome_0 = display.submit_frame(
        &intro_0.first_frame_header,
        sardp::timecode_frame::SyntheticFrame {
            width: 1,
            height: 1,
            rgb: vec![0, 0, 0],
        },
    );
    assert_eq!(outcome_0, SubmitOutcome::Displayed);
    let outcome_1 = display.submit_frame(
        &intro_1.first_frame_header,
        sardp::timecode_frame::SyntheticFrame {
            width: 1,
            height: 1,
            rgb: vec![255, 255, 255],
        },
    );
    assert_eq!(outcome_1, SubmitOutcome::Displayed);
    assert_eq!(display.current_generation(), Some(1));
}

#[tokio::test]
async fn high_rtt_with_no_congestion_never_enters_congested() {
    // DR-029's whole reason for existing: on the *old* absolute-threshold
    // design, a stable 300ms client_queue_delay_us (a plausible
    // intercontinental/satellite RTT contribution, no congestion at all)
    // would have exceeded a 100ms absolute threshold and misfired into
    // Congested/Recovering forever. The delta-from-rolling-baseline
    // design must not.
    let (client_connection, server_connection) = connect_pair().await;

    let (server_send, client_intro) = tokio::join!(
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
    let (_send_stream, _instance_sm) = server_send.expect("server opens generation 0");
    client_intro.expect("client receives generation 0");

    let mut channel = VideoChannel::new(0);
    channel.mark_instance_streaming().unwrap();

    let mut feedback_send = open_feedback_stream(&client_connection)
        .await
        .expect("client opens feedback stream");
    let mut feedback_recv = FeedbackReceiver::accept(&server_connection)
        .await
        .expect("server accepts feedback stream");

    // 15 seconds of simulated feedback at the spec's 100ms cadence: long
    // enough for the 10s baseline window to fully absorb and converge on
    // the stable high delay.
    let mut now_us = 0u64;
    for _ in 0..150 {
        let decision = feed_sample(
            &mut feedback_send,
            &mut feedback_recv,
            &mut channel,
            now_us,
            300_000, // stable 300ms -- would trip a 100ms *absolute* threshold
        )
        .await;
        assert_eq!(
            decision,
            BackpressureDecision::Continue,
            "must not enter Congested under a high-but-stable delay (DR-029)"
        );
        now_us += 100_000;
    }

    assert_eq!(channel.instance_state(), InstanceState::Streaming);
    assert_eq!(channel.channel_state(), ChannelState::Live);
    assert_eq!(
        channel.generation(),
        0,
        "no reset should ever have occurred"
    );
    assert_eq!(
        channel.baseline_us(),
        Some(300_000),
        "baseline should have converged to the stable delay, keeping delta ~0"
    );
}
