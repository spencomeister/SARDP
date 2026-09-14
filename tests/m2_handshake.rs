//! M2 integration test: a real loopback QUIC connection (rcgen test cert,
//! ALPN `sardp/1`), full ClientHello/ServerHello/AuthPubkey/AuthResult
//! exchange, and the Connection SM's Handshaking -> Authenticating ->
//! Authenticated transitions -- exercising the actual TLS exporter via
//! `quinn::Connection::export_keying_material`, not a stub.

mod common;

use common::{connect_pair, handshake_pair};

use std::time::Duration;

use ed25519_dalek::SigningKey;
use sardp::connection_sm::ConnectionState;
use sardp::handshake::{
    HandshakeError, client_handshake, client_handshake_with_timeouts, server_handshake,
};
use sardp::stream_kind::StreamKind;

#[tokio::test]
async fn quic_connection_negotiates_sardp_alpn() {
    let (client_connection, server_connection) = connect_pair().await;
    assert_eq!(
        client_connection.handshake_data().is_some(),
        server_connection.handshake_data().is_some()
    );
}

#[tokio::test]
async fn full_handshake_reaches_authenticated_on_both_sides() {
    // The shared helper drives both sides with `join!` (see its doc for
    // why not `tokio::spawn`); `_client_connection`/`_server_connection`
    // are kept alive here for the whole exchange the same way.
    let (_client_connection, client, _server_connection, server) = handshake_pair(0x11).await;

    assert_eq!(client.sm.state(), ConnectionState::Authenticated);
    assert_eq!(server.sm.state(), ConnectionState::Authenticated);
    assert_eq!(client.outcome.session_id, server.outcome.session_id);
    assert_eq!(
        client.outcome.reconnect_token,
        server.outcome.reconnect_token
    );
    assert_eq!(
        client.outcome.granted_permissions,
        server.outcome.granted_permissions
    );
}

#[tokio::test]
async fn handshake_with_untrusted_key_is_denied() {
    let (client_connection, server_connection) = connect_pair().await;

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
async fn handshake_timeout_fires_if_server_never_responds() {
    // Spec 4.7 HANDSHAKE_TIMEOUT (10s in production, `client_handshake`'s
    // default): proves the `tokio::time::timeout` wiring in
    // `client_handshake_with_timeouts` actually elapses and surfaces
    // `HandshakeError::HandshakeTimeout`, using a tiny override so the
    // test itself stays fast rather than waiting out the real 10s value.
    let (client_connection, server_connection) = connect_pair().await;

    let unresponsive_server = async move {
        // Accepts the control stream (so the client's writes aren't stuck
        // behind an unaccepted stream) but never reads or sends anything
        // on it. Binding (not discarding) the accepted streams matters:
        // dropping an unfinished `SendStream` implicitly resets it, which
        // would deliver the client an early, unrelated stream-reset error
        // instead of genuinely exercising the timeout.
        let _accepted = server_connection.accept_bi().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    };

    let client_signing_key = SigningKey::from_bytes(&[0x66; 32]);
    let (client_result, ()) = tokio::join!(
        client_handshake_with_timeouts(
            &client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
            Duration::from_millis(50),
            Duration::from_secs(60),
        ),
        unresponsive_server,
    );

    assert!(matches!(
        client_result,
        Err(HandshakeError::HandshakeTimeout)
    ));
}

#[tokio::test]
async fn auth_timeout_fires_if_server_never_sends_auth_result() {
    // Spec 4.7 AUTH_TIMEOUT: same proof as the HANDSHAKE_TIMEOUT test
    // above, but for the second phase -- a server that completes the
    // ClientHello/ServerHello exchange (so the client legitimately enters
    // Authenticating) and then goes silent instead of ever sending
    // AuthResult.
    let (client_connection, server_connection) = connect_pair().await;

    let stalls_after_server_hello = async move {
        let (mut send, recv) = server_connection.accept_bi().await.unwrap();
        let mut reader = sardp::stream_reader::EnvelopeReader::new(recv);
        reader.read_prologue().await.unwrap();
        reader
            .read_envelope(StreamKind::Control.max_envelope_length())
            .await
            .unwrap(); // ClientHello

        let server_hello = sardp::messages::ServerHello {
            server_name: "stalling-server".into(),
            server_version: "0".into(),
            capabilities: vec![],
            auth_policy: sardp::messages::AuthPolicy {
                accepted_combinations: vec![],
            },
            auth_challenge: [0u8; 32],
        };
        sardp::stream_reader::write_envelope(
            &mut send,
            sardp::messages::type_id::SERVER_HELLO,
            &sardp::messages::encode(&server_hello),
        )
        .await
        .unwrap();

        // Never reads AuthPubkey, never sends AuthResult.
        tokio::time::sleep(Duration::from_millis(300)).await;
    };

    let client_signing_key = SigningKey::from_bytes(&[0x77; 32]);
    let (client_result, ()) = tokio::join!(
        client_handshake_with_timeouts(
            &client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
            Duration::from_secs(5),
            Duration::from_millis(50),
        ),
        stalls_after_server_hello,
    );

    assert!(matches!(client_result, Err(HandshakeError::AuthTimeout)));
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
