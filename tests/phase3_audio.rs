//! Phase 3 integration test: audio (spec 2.13) over real loopback QUIC.
//! Covers both direction-specific streams (`audio_playback`,
//! server->client; `audio_capture`, client->server) sharing the same
//! `AudioConfig`/`AudioFrame` wire shape, and `AudioSyncFeedback` riding
//! the existing `feedback` stream alongside `TransportFeedback`. Uses
//! pseudo audio data (a synthetic sine wave, standing in for Opus, plus a
//! silence buffer) rather than a real audio device or Opus encoder.

use std::f64::consts::PI;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sardp::audio_session::{
    accept_audio_capture_gated, accept_audio_stream, open_audio_stream, send_audio_frame,
};
use sardp::feedback_session::{FeedbackMessage, FeedbackReceiver, send_audio_sync_feedback};
use sardp::messages::{AudioCodec, AudioConfig, AudioSyncFeedback};
use sardp::stream_kind::StreamKind;
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

fn test_audio_config() -> AudioConfig {
    AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48_000,
        channels: 1,
        frame_duration_ms: 20,
    }
}

/// A synthetic 440Hz sine wave, 16-bit PCM-shaped bytes standing in for a
/// real Opus payload (this PoC doesn't run a real encoder, per scope).
fn sine_wave_payload(samples: usize) -> Vec<u8> {
    (0..samples)
        .flat_map(|i| {
            let t = i as f64 / 48_000.0;
            let sample = (i16::MAX as f64 * 0.5 * (2.0 * PI * 440.0 * t).sin()) as i16;
            sample.to_le_bytes()
        })
        .collect()
}

fn silence_payload(samples: usize) -> Vec<u8> {
    vec![0u8; samples * 2]
}

/// `audio_playback` (spec 2.2.1: server-initiated, server->client).
#[tokio::test]
async fn audio_playback_round_trips_sine_wave_frames() {
    let (client_connection, server_connection) = connect_pair().await;
    let config = test_audio_config();

    let (open_result, accept_result) = tokio::join!(
        open_audio_stream(&server_connection, StreamKind::AudioPlayback, &config),
        accept_audio_stream(&client_connection, StreamKind::AudioPlayback),
    );
    let mut send = open_result.expect("server opens audio_playback");
    let (received_config, mut frame_reader) = accept_result.expect("client accepts audio_playback");
    assert_eq!(received_config, config);

    let frames = [
        (0u64, 0u64, sine_wave_payload(960)),
        (1u64, 20_000u64, sine_wave_payload(960)),
        (2u64, 40_000u64, silence_payload(960)),
    ];

    let (send_result, read_result) = tokio::join!(
        async {
            for (sequence, capture_ts, payload) in &frames {
                send_audio_frame(&mut send, *sequence, *capture_ts, 20_000, payload).await?;
            }
            Ok::<_, sardp::audio_session::AudioError>(())
        },
        async {
            let mut received = Vec::new();
            for _ in 0..frames.len() {
                received.push(frame_reader.read_next_frame().await?);
            }
            Ok::<_, sardp::audio_session::AudioError>(received)
        },
    );
    send_result.expect("server sends all frames");
    let received = read_result.expect("client reads all frames");

    for ((sequence, capture_ts, payload), (header, received_payload)) in
        frames.iter().zip(received.iter())
    {
        assert_eq!(header.sequence, *sequence);
        assert_eq!(header.capture_ts, *capture_ts);
        assert_eq!(header.duration_us, 20_000);
        assert_eq!(header.payload_len, payload.len() as u64);
        assert_eq!(received_payload, payload);
    }
}

/// `audio_capture` (spec 2.2.1: client-initiated, client->server) --
/// the same message shapes and functions, just the opposite direction and
/// initiator, proving the direction-parameterized module actually
/// respects spec 2.2.1's initiator table rather than only working one way.
#[tokio::test]
async fn audio_capture_round_trips_silence_frames() {
    let (client_connection, server_connection) = connect_pair().await;
    let config = test_audio_config();

    let (open_result, accept_result) = tokio::join!(
        open_audio_stream(&client_connection, StreamKind::AudioCapture, &config),
        accept_audio_stream(&server_connection, StreamKind::AudioCapture),
    );
    let mut send = open_result.expect("client opens audio_capture");
    let (received_config, mut frame_reader) = accept_result.expect("server accepts audio_capture");
    assert_eq!(received_config, config);

    let payload = silence_payload(960);
    let (send_result, read_result) = tokio::join!(
        send_audio_frame(&mut send, 0, 0, 20_000, &payload),
        frame_reader.read_next_frame(),
    );
    send_result.expect("client sends a frame");
    let (header, received_payload) = read_result.expect("server reads the frame");
    assert_eq!(header.sequence, 0);
    assert_eq!(header.payload_len, payload.len() as u64);
    assert_eq!(received_payload, payload);
}

/// A stream opened as the wrong kind is rejected rather than silently
/// accepted as if it were audio.
#[tokio::test]
async fn accepting_the_wrong_stream_kind_is_rejected() {
    let (client_connection, server_connection) = connect_pair().await;
    let config = test_audio_config();

    let (open_result, accept_result) = tokio::join!(
        open_audio_stream(&server_connection, StreamKind::AudioPlayback, &config),
        accept_audio_stream(&client_connection, StreamKind::AudioCapture),
    );
    open_result.expect("server opens the stream regardless");
    assert!(matches!(
        accept_result,
        Err(sardp::audio_session::AudioError::WrongStreamKind)
    ));
}

/// KNOWN_ISSUES.md #12: `sardp-server`'s real wiring refuses an
/// `audio_capture` stream rather than accepting it when `AUDIO_CAPTURE`
/// isn't granted -- proven here at the library level `accept_audio_capture_gated`
/// exists for.
#[tokio::test]
async fn audio_capture_is_accepted_when_granted() {
    let (client_connection, server_connection) = connect_pair().await;
    let config = test_audio_config();

    let (open_result, accept_result) = tokio::join!(
        open_audio_stream(&client_connection, StreamKind::AudioCapture, &config),
        accept_audio_capture_gated(&server_connection, true),
    );
    open_result.expect("client opens audio_capture");
    let (received_config, _frame_reader) = accept_result
        .expect("accept succeeds")
        .expect("granted, so Some(..) not None");
    assert_eq!(received_config, config);
}

#[tokio::test]
async fn audio_capture_is_refused_when_not_granted() {
    let (client_connection, server_connection) = connect_pair().await;
    let config = test_audio_config();

    let (open_result, accept_result) = tokio::join!(
        open_audio_stream(&client_connection, StreamKind::AudioCapture, &config),
        accept_audio_capture_gated(&server_connection, false),
    );
    open_result.expect("client opens audio_capture regardless -- permission is the server's call");
    let outcome = accept_result.expect("accept itself succeeds; the stream is merely refused");
    assert!(
        outcome.is_none(),
        "AUDIO_CAPTURE not granted must yield None, not a usable reader"
    );
}

/// `AudioSyncFeedback` (spec 2.13) shares the `feedback` stream with
/// `TransportFeedback` (spec 2.14); `FeedbackReceiver::read_message`
/// dispatches on which one arrived.
#[tokio::test]
async fn audio_sync_feedback_is_read_off_the_shared_feedback_stream() {
    let (client_connection, server_connection) = connect_pair().await;

    let (open_result, accept_result) = tokio::join!(
        sardp::feedback_session::open_feedback_stream(&client_connection),
        FeedbackReceiver::accept(&server_connection),
    );
    let mut send = open_result.expect("client opens feedback stream");
    let mut receiver = accept_result.expect("server accepts feedback stream");

    let feedback = AudioSyncFeedback {
        audio_played_ts: 5_000_000,
        video_displayed_ts: 4_998_500,
        drift_ppm: -12,
    };
    let (send_result, read_result) = tokio::join!(
        send_audio_sync_feedback(&mut send, &feedback),
        receiver.read_message(),
    );
    send_result.expect("client sends AudioSyncFeedback");
    let message = read_result.expect("server reads it");
    match message {
        FeedbackMessage::AudioSync(received) => assert_eq!(received, feedback),
        FeedbackMessage::Transport(_) => panic!("expected AudioSync, got Transport"),
        FeedbackMessage::Keyframe(_) => panic!("expected AudioSync, got KeyframeRequest"),
    }
}
