//! Shared setup for the loopback QUIC integration tests: endpoints, the
//! bare QUIC connection pair every test starts from, and the standard
//! successful M2 handshake on top of it.
//!
//! This file is compiled into every test binary that declares
//! `mod common;`, so helpers a given binary doesn't use would trip
//! `dead_code`; hence the blanket allow.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ed25519_dalek::SigningKey;
use sardp::connection_sm::ConnectionSm;
use sardp::handshake::{ControlChannel, HandshakeOutcome, client_handshake, server_handshake};
use sardp::{net, pki};

/// Local bind address for the test endpoints. Defaults to 127.0.0.1, but
/// honors `SARDP_TEST_BIND_ADDR` (an IPv4 address) so the tests can still
/// run on a machine whose UDP *loopback* is broken while UDP over a real
/// local interface works (KNOWN_ISSUES.md item 14 -- e.g. set it to the
/// machine's LAN address).
pub fn loopback(port: u16) -> SocketAddr {
    let ip = std::env::var("SARDP_TEST_BIND_ADDR")
        .ok()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .unwrap_or(Ipv4Addr::LOCALHOST);
    SocketAddr::new(IpAddr::V4(ip), port)
}

/// A server endpoint on an ephemeral port with a fresh self-signed test
/// certificate. Keep it around when a test needs *several* connections
/// to the same server (the reconnection tests: suspend on one QUIC
/// connection, resume on a brand new one); [`connect_pair`] is the
/// one-connection shorthand.
pub struct TestServer {
    pub endpoint: quinn::Endpoint,
    pub addr: SocketAddr,
    pub cert: pki::TestCertificate,
}

impl TestServer {
    pub fn start() -> Self {
        let cert = pki::generate_test_certificate("localhost");
        let endpoint = net::server_endpoint(loopback(0), &cert);
        let addr = endpoint.local_addr().unwrap();
        Self {
            endpoint,
            addr,
            cert,
        }
    }
}

/// Opens one new QUIC connection to `server` from a fresh client
/// endpoint and returns both ends once the QUIC handshake completes:
/// `(client_connection, server_connection)`.
pub async fn connect_to(server: &TestServer) -> (quinn::Connection, quinn::Connection) {
    let client_endpoint = net::client_endpoint(loopback(0), &server.cert.cert_der);
    let server_endpoint = server.endpoint.clone();
    let server_accept = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.expect("incoming connection");
        incoming.await.expect("server-side QUIC handshake")
    });
    let client_connection = client_endpoint
        .connect(server.addr, "localhost")
        .expect("valid connect params")
        .await
        .expect("client-side QUIC handshake");
    let server_connection = server_accept.await.unwrap();
    (client_connection, server_connection)
}

/// A single connected QUIC pair, `(client_connection, server_connection)`,
/// to a throwaway server. Both `Connection`s stay usable after the
/// endpoints behind them go out of scope here.
pub async fn connect_pair() -> (quinn::Connection, quinn::Connection) {
    let server = TestServer::start();
    connect_to(&server).await
}

/// One side's results from a successful M2 handshake.
pub struct HandshakeSide {
    pub outcome: HandshakeOutcome,
    pub sm: ConnectionSm,
    pub control: ControlChannel,
}

/// Runs the standard successful handshake (client "test-client" / user
/// "alice" / "device-1" against server "test-server", the client signing
/// with the key derived from `key_seed`, which the server trusts) over an
/// already-connected pair and returns `(client, server)` sides.
///
/// `join!` (not `tokio::spawn`) keeps both `Connection`s alive in the
/// caller's scope for the whole exchange. Spawning each handshake into
/// its own task, by contrast, drops that task's `Connection` the moment
/// its future resolves -- and quinn's `Connection` sends an implicit
/// `ApplicationClose(0, "")` on last-handle drop, which can race the
/// final `AuthResult` write and blow away the peer's read of it before
/// it's actually flushed. The real analogue of `join!` here is that a
/// real caller keeps the session's `Connection` alive for as long as the
/// session lasts, not just for the handshake call.
pub async fn handshake(
    client_connection: &quinn::Connection,
    server_connection: &quinn::Connection,
    key_seed: u8,
) -> (HandshakeSide, HandshakeSide) {
    let client_signing_key = SigningKey::from_bytes(&[key_seed; 32]);
    let trusted_public_key = client_signing_key.verifying_key();
    let (client_result, server_result) = tokio::join!(
        client_handshake(
            client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        server_handshake(server_connection, "test-server", &trusted_public_key),
    );
    let (client_outcome, client_sm, client_control) =
        client_result.expect("client handshake succeeds");
    let (server_outcome, server_sm, server_control) =
        server_result.expect("server handshake succeeds");
    (
        HandshakeSide {
            outcome: client_outcome,
            sm: client_sm,
            control: client_control,
        },
        HandshakeSide {
            outcome: server_outcome,
            sm: server_sm,
            control: server_control,
        },
    )
}

/// [`connect_pair`] followed by [`handshake`]: a session with the
/// `control` stream established on both ends, ready for whatever the
/// test wants to exercise on top of M2. Returns
/// `(client_connection, client, server_connection, server)`.
pub async fn handshake_pair(
    key_seed: u8,
) -> (
    quinn::Connection,
    HandshakeSide,
    quinn::Connection,
    HandshakeSide,
) {
    let (client_connection, server_connection) = connect_pair().await;
    let (client, server) = handshake(&client_connection, &server_connection, key_seed).await;
    (client_connection, client, server_connection, server)
}
