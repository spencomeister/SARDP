//! KNOWN_ISSUES.md #6: `sardp::reconnection::establish_connection`'s
//! `ClientHello`-wait ([`sardp::reconnection::read_first_control_message`])
//! and, on that path, `sardp::handshake::server_handshake_from_client_hello`'s
//! `ServerHello` send now share a single spec 4.7 `HANDSHAKE_TIMEOUT`
//! deadline (a `tokio::time::Instant` computed once and threaded through),
//! rather than each independently getting a fresh `handshake_timeout`
//! window -- which used to let the theoretical worst case stretch to
//! `2 * HANDSHAKE_TIMEOUT`. Both functions now take that shared `Instant`
//! directly (not a `Duration`), so there's no way left to accidentally
//! recompute a fresh window inside either of them.
//!
//! Only `read_first_control_message`'s half is exercised here by actually
//! forcing a timeout: it genuinely blocks on network input (the peer's
//! next Envelope), so a deadline that's already elapsed must fail it
//! immediately rather than granting a fresh `handshake_timeout` wait.
//! `server_handshake_from_client_hello`'s `ServerHello` send has no
//! equivalent real blocking point to force from outside in a test (a
//! small write over a fresh QUIC stream's initial flow-control window
//! completes without ever yielding, so `tokio::time::timeout_at` never
//! gets a chance to observe an elapsed deadline there) -- its contribution
//! to the fix is the removal of its own independent `handshake_timeout`
//! window, proven by its signature no longer accepting a `Duration` to
//! recompute one from, not by a runtime race.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use sardp::handshake::HandshakeError;
use sardp::reconnection::read_first_control_message;
use sardp::stream_kind::StreamKind;
use sardp::{net, pki, prologue};

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
        incoming.await.expect("server-side QUIC handshake")
    });
    let client_connection = client_endpoint
        .connect(server_addr, "localhost")
        .expect("valid connect params")
        .await
        .expect("client-side QUIC handshake");
    let server_connection = server_accept.await.unwrap();
    (client_connection, server_connection)
}

/// `establish_connection` computes the shared deadline once and passes it
/// straight to `read_first_control_message`; an already-elapsed deadline
/// must fail this wait immediately, not wait out a fresh
/// `handshake_timeout` -- the property that keeps a slow-to-send-ClientHello
/// peer from also buying a second full window for whatever comes after.
#[tokio::test]
async fn read_first_control_message_honors_an_already_elapsed_shared_deadline() {
    let (client_connection, server_connection) = connect_pair().await;

    let client_task = tokio::spawn(async move {
        let (mut send, _recv) = client_connection.open_bi().await.unwrap();
        let mut prologue_bytes = Vec::new();
        prologue::encode(StreamKind::Control, 1, 0, &mut prologue_bytes);
        send.write_all(&prologue_bytes).await.unwrap();
        // Deliberately never sends ClientHello -- irrelevant here, since
        // the deadline is already elapsed before this side would even
        // start waiting for it.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let already_elapsed = tokio::time::Instant::now() - Duration::from_secs(1);
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        read_first_control_message(&server_connection, already_elapsed),
    )
    .await
    .expect("must fail well within the outer 500ms bound, not hang");

    assert!(matches!(result, Err(HandshakeError::HandshakeTimeout)));
    client_task.await.unwrap();
}
