//! Phase 1 Part 3 integration test: the Reconnection SM (spec 4.6) over
//! real loopback QUIC. A session is established and reaches Active, the
//! QUIC connection is then genuinely dropped (not a graceful
//! `SessionClose`), and a brand new connection resumes the same session
//! via `SessionReauthenticate` + the `reconnect_token` issued at the
//! original handshake -- confirming: `Active -> Suspended -> Active`
//! (spec 4.1), the video Channel's generation continues rather than
//! resetting (spec 4.6, DR-026), the `reconnect_token` is rotated, and
//! that a wrong/reused token is rejected (`ReconnectRejected`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ed25519_dalek::SigningKey;
use sardp::connection_sm::ConnectionState;
use sardp::handshake::{client_handshake, server_handshake};
use sardp::messages;
use sardp::reconnection::{accept_control_prologue, client_reconnect, server_complete_reconnect};
use sardp::session_store::{SessionStore, SuspendedSession};
use sardp::stream_kind::StreamKind;
use sardp::{net, pki};

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Accepts one incoming connection on `endpoint` (cloned -- `quinn::Endpoint`
/// is a cheap `Arc`-backed handle, so the same listening socket can accept
/// more than one connection across a test's lifetime).
async fn accept_one(endpoint: &quinn::Endpoint) -> quinn::Connection {
    let incoming = endpoint.accept().await.expect("incoming connection");
    incoming.await.expect("server-side QUIC handshake")
}

#[tokio::test]
async fn reconnect_after_disconnect_resumes_active_with_continued_generation() {
    let test_cert = pki::generate_test_certificate("localhost");
    let server_endpoint = net::server_endpoint(loopback(0), &test_cert);
    let server_addr = server_endpoint.local_addr().unwrap();

    let client_signing_key = SigningKey::from_bytes(&[0x81; 32]);
    let trusted_public_key = client_signing_key.verifying_key();

    // --- Phase 1: establish the original session and reach Active. ---
    let client_endpoint_1 = net::client_endpoint(loopback(0), &test_cert.cert_der);
    let (client_connection_1, server_connection_1) = {
        let server_endpoint = server_endpoint.clone();
        let server_accept = tokio::spawn(async move { accept_one(&server_endpoint).await });
        let client_connection = client_endpoint_1
            .connect(server_addr, "localhost")
            .expect("valid connect params")
            .await
            .expect("client-side QUIC handshake");
        (client_connection, server_accept.await.unwrap())
    };

    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection_1,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        server_handshake(&server_connection_1, "test-server", &trusted_public_key),
    );
    let (client_outcome, mut client_sm, _client_control_1) =
        client_result.expect("client handshake succeeds");
    let (server_outcome, mut server_sm, _server_control_1) =
        server_result.expect("server handshake succeeds");

    client_sm.on_channel_live().expect("client reaches Active");
    server_sm.on_channel_live().expect("server reaches Active");
    assert_eq!(server_sm.state(), ConnectionState::Active);

    let session_id = server_outcome.session_id;
    assert_eq!(client_outcome.session_id, session_id);
    let original_reconnect_token = client_outcome.reconnect_token;
    assert_eq!(server_outcome.reconnect_token, original_reconnect_token);

    // Pretend this Channel already went through a couple of
    // backpressure-triggered reopens (M5) before the disconnect, so the
    // continuity check below is meaningful rather than a trivial 0 -> 1.
    let generation_before_disconnect = 3u64;

    // --- Phase 2: the connection is genuinely lost (not a graceful
    // SessionClose) -- drop is the same "implicit ApplicationClose"
    // mechanism this codebase's other tests document, used here on
    // purpose to simulate a real network drop. ---
    server_sm.suspend().expect("Active -> Suspended");
    client_sm.suspend().expect("Active -> Suspended");

    let session_store = SessionStore::new();
    session_store.suspend(
        session_id,
        SuspendedSession {
            reconnect_token: original_reconnect_token,
            connection_sm: server_sm,
            granted_permissions: server_outcome.granted_permissions,
            last_generation: generation_before_disconnect,
        },
    );

    drop(client_connection_1);
    drop(server_connection_1);

    // --- Phase 3: reconnect on a brand new QUIC connection within the
    // grace period, using the reconnect_token from the original session. ---
    let client_endpoint_2 = net::client_endpoint(loopback(0), &test_cert.cert_der);
    let (client_connection_2, server_connection_2) = {
        let server_endpoint = server_endpoint.clone();
        let server_accept = tokio::spawn(async move { accept_one(&server_endpoint).await });
        let client_connection = client_endpoint_2
            .connect(server_addr, "localhost")
            .expect("valid connect params")
            .await
            .expect("client-side QUIC handshake");
        (client_connection, server_accept.await.unwrap())
    };

    let server_side_reconnect = async {
        let (send, mut reader) = accept_control_prologue(&server_connection_2)
            .await
            .expect("prologue accepted");
        let (type_raw, payload) = reader
            .read_envelope(StreamKind::Control.max_envelope_length())
            .await
            .expect("reads the SessionReauthenticate envelope");
        assert_eq!(type_raw, messages::type_id::SESSION_REAUTHENTICATE);
        let reauth: messages::SessionReauthenticate =
            messages::decode(&payload).expect("decodes SessionReauthenticate");
        assert_eq!(reauth.reason, messages::ReauthenticateReason::Reconnect);
        assert_eq!(reauth.prior_session_id, session_id);
        server_complete_reconnect(send, reader, &reauth, &session_store).await
    };

    let (client_reconnect_result, server_reconnect_result) = tokio::join!(
        client_reconnect(
            &client_connection_2,
            &mut client_sm,
            session_id,
            original_reconnect_token,
        ),
        server_side_reconnect,
    );

    let (client_reconnect_outcome, _client_control_2) =
        client_reconnect_result.expect("client reconnect succeeds");
    let (server_reconnect_outcome, resumed_server_sm, _server_control_2) =
        server_reconnect_result.expect("server reconnect succeeds");

    // Both sides are back to Active (spec 4.6: "ReconnectAccepted ->
    // [Connection SM: Active]"), without re-running the handshake.
    assert_eq!(client_sm.state(), ConnectionState::Active);
    assert_eq!(resumed_server_sm.state(), ConnectionState::Active);

    // DR-026: generation continues monotonically, it does not reset.
    assert_eq!(
        server_reconnect_outcome.resumed_generation,
        generation_before_disconnect + 1
    );

    // Spec 4.6: "旧reconnect_token失効、新トークン発行" -- the token is
    // rotated on every successful reconnect.
    assert_ne!(
        client_reconnect_outcome.reconnect_token,
        original_reconnect_token
    );
    assert_eq!(
        client_reconnect_outcome.reconnect_token,
        server_reconnect_outcome.reconnect_token
    );
    assert_eq!(client_reconnect_outcome.session_id, session_id);
    assert_eq!(
        client_reconnect_outcome.granted_permissions,
        server_outcome.granted_permissions
    );

    // The old token is spent: it cannot be replayed for a second reconnect.
    assert!(!session_store.contains(session_id));
}

#[tokio::test]
async fn reconnect_with_wrong_token_is_rejected() {
    let test_cert = pki::generate_test_certificate("localhost");
    let server_endpoint = net::server_endpoint(loopback(0), &test_cert);
    let server_addr = server_endpoint.local_addr().unwrap();

    let client_signing_key = SigningKey::from_bytes(&[0x82; 32]);
    let trusted_public_key = client_signing_key.verifying_key();

    let client_endpoint_1 = net::client_endpoint(loopback(0), &test_cert.cert_der);
    let (client_connection_1, server_connection_1) = {
        let server_endpoint = server_endpoint.clone();
        let server_accept = tokio::spawn(async move { accept_one(&server_endpoint).await });
        let client_connection = client_endpoint_1
            .connect(server_addr, "localhost")
            .expect("valid connect params")
            .await
            .expect("client-side QUIC handshake");
        (client_connection, server_accept.await.unwrap())
    };
    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection_1,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        server_handshake(&server_connection_1, "test-server", &trusted_public_key),
    );
    let (client_outcome, mut client_sm, _client_control_1) = client_result.unwrap();
    let (server_outcome, mut server_sm, _server_control_1) = server_result.unwrap();
    client_sm.on_channel_live().unwrap();
    server_sm.on_channel_live().unwrap();

    let session_id = server_outcome.session_id;
    server_sm.suspend().unwrap();
    client_sm.suspend().unwrap();

    let session_store = SessionStore::new();
    session_store.suspend(
        session_id,
        SuspendedSession {
            reconnect_token: client_outcome.reconnect_token,
            connection_sm: server_sm,
            granted_permissions: server_outcome.granted_permissions,
            last_generation: 0,
        },
    );
    drop(client_connection_1);
    drop(server_connection_1);

    let client_endpoint_2 = net::client_endpoint(loopback(0), &test_cert.cert_der);
    let (client_connection_2, server_connection_2) = {
        let server_endpoint = server_endpoint.clone();
        let server_accept = tokio::spawn(async move { accept_one(&server_endpoint).await });
        let client_connection = client_endpoint_2
            .connect(server_addr, "localhost")
            .expect("valid connect params")
            .await
            .expect("client-side QUIC handshake");
        (client_connection, server_accept.await.unwrap())
    };

    let wrong_token = {
        let mut t = client_outcome.reconnect_token;
        t[0] ^= 0xFF;
        t
    };

    let server_side_reconnect = async {
        let (send, mut reader) = accept_control_prologue(&server_connection_2).await.unwrap();
        let (_type_raw, payload) = reader
            .read_envelope(StreamKind::Control.max_envelope_length())
            .await
            .unwrap();
        let reauth: messages::SessionReauthenticate = messages::decode(&payload).unwrap();
        server_complete_reconnect(send, reader, &reauth, &session_store).await
    };

    let (client_reconnect_result, server_reconnect_result) = tokio::join!(
        client_reconnect(
            &client_connection_2,
            &mut client_sm,
            session_id,
            wrong_token
        ),
        server_side_reconnect,
    );

    assert!(matches!(
        client_reconnect_result,
        Err(sardp::handshake::HandshakeError::AuthDenied)
    ));
    assert!(server_reconnect_result.is_err());
    // Client stays Suspended: the reconnect attempt failed, so `resume()`
    // was never called.
    assert_eq!(client_sm.state(), ConnectionState::Suspended);
    // The legitimate session is untouched and can still be reconnected to.
    assert!(session_store.contains(session_id));
}
