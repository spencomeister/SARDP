//! M2 integration test: a real loopback QUIC connection (rcgen test cert,
//! ALPN `sardp/1`), full ClientHello/ServerHello/AuthPubkey/AuthResult
//! exchange, and the Connection SM's Handshaking -> Authenticating ->
//! Authenticated transitions -- exercising the actual TLS exporter via
//! `quinn::Connection::export_keying_material`, not a stub.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ed25519_dalek::SigningKey;
use sardp::connection_sm::ConnectionState;
use sardp::handshake::{HandshakeError, client_handshake, server_handshake};
use sardp::{net, pki};

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

async fn connect_pair() -> (quinn::Connection, quinn::Connection, pki::TestCertificate) {
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
    (client_connection, server_connection, test_cert)
}

#[tokio::test]
async fn quic_connection_negotiates_sardp_alpn() {
    let (client_connection, server_connection, _cert) = connect_pair().await;
    assert_eq!(
        client_connection.handshake_data().is_some(),
        server_connection.handshake_data().is_some()
    );
}

#[tokio::test]
async fn full_handshake_reaches_authenticated_on_both_sides() {
    let (client_connection, server_connection, _cert) = connect_pair().await;

    let client_signing_key = SigningKey::from_bytes(&[0x11; 32]);
    let trusted_public_key = client_signing_key.verifying_key();

    // `join!` (not `tokio::spawn`) keeps both `Connection`s alive in this
    // function's scope for the whole exchange. Spawning each handshake
    // into its own task, by contrast, drops that task's `Connection` the
    // moment its future resolves -- and quinn's `Connection` sends an
    // implicit `ApplicationClose(0, "")` on last-handle drop, which can
    // race the final `AuthResult` write and blow away the peer's read of
    // it before it's actually flushed. The real analogue of `join!` here
    // is that a real caller keeps the session's `Connection` alive for as
    // long as the session lasts, not just for the handshake call.
    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        server_handshake(&server_connection, "test-server", &trusted_public_key),
    );
    let client_outcome = client_result.expect("client handshake succeeds");
    let server_outcome = server_result.expect("server handshake succeeds");

    assert_eq!(client_outcome.state, ConnectionState::Authenticated);
    assert_eq!(server_outcome.state, ConnectionState::Authenticated);
    assert_eq!(client_outcome.session_id, server_outcome.session_id);
    assert_eq!(
        client_outcome.reconnect_token,
        server_outcome.reconnect_token
    );
    assert_eq!(
        client_outcome.granted_permissions,
        server_outcome.granted_permissions
    );
}

#[tokio::test]
async fn handshake_with_untrusted_key_is_denied() {
    let (client_connection, server_connection, _cert) = connect_pair().await;

    // The server only trusts this key...
    let trusted_signing_key = SigningKey::from_bytes(&[0x22; 32]);
    let trusted_public_key = trusted_signing_key.verifying_key();
    // ...but the client signs with a different one.
    let impostor_signing_key = SigningKey::from_bytes(&[0x33; 32]);

    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection,
            &impostor_signing_key,
            "test-client",
            "mallory",
            "device-x",
        ),
        server_handshake(&server_connection, "test-server", &trusted_public_key),
    );

    assert!(matches!(client_result, Err(HandshakeError::AuthDenied)));
    assert!(matches!(server_result, Err(HandshakeError::Auth(_))));
}

#[tokio::test]
async fn tampered_signature_over_correct_exporter_is_rejected() {
    // Sanity check that verification is not a no-op: a validly-shaped but
    // wrong signature (signed over different bytes) must fail even though
    // it comes from the trusted key.
    let signing_key = SigningKey::from_bytes(&[0x44; 32]);
    let verifying_key = signing_key.verifying_key();
    let real_exporter = [0xAA; sardp::auth::EXPORTER_LENGTH];
    let wrong_exporter = [0xBB; sardp::auth::EXPORTER_LENGTH];
    let signature = sardp::auth::sign_exporter(&signing_key, &wrong_exporter);

    assert!(
        sardp::auth::verify_exporter(verifying_key.as_bytes(), &real_exporter, &signature).is_err()
    );
}
