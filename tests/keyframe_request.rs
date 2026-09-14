//! `KeyframeRequest` (spec 2.10) on the `feedback` stream over real
//! loopback QUIC: the client's queue circuit breaker
//! (`sardp::queue_circuit_breaker`) sends it on the same stream that
//! carries `TransportFeedback`, and a server reading that stream with
//! `FeedbackReceiver::read_message` must see both, in order, without
//! tripping `UnexpectedType`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sardp::feedback_session::{
    FeedbackMessage, FeedbackReceiver, open_feedback_stream, send_keyframe_request,
    send_transport_feedback,
};
use sardp::messages::{KeyframeReason, KeyframeRequest, TransportFeedback};
use sardp::queue_circuit_breaker::{ClientQueueCircuitBreaker, defaults};
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
