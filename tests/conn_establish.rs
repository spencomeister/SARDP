//! `establish_connection` (spec 4.6) over real loopback QUIC: the unified
//! accept-loop dispatch `sardp-server` uses to tell a fresh `ClientHello`
//! apart from a `SessionReauthenticate` and drive either all the way to
//! the Active-state threshold. Extracted into the library crate
//! (KNOWN_ISSUES.md #1) precisely so this dispatch itself -- previously
//! only exercised by hand-running two real binaries -- gets a real
//! automated regression test.

mod common;

use common::{TestServer, connect_to};

use ed25519_dalek::SigningKey;
use sardp::connection_sm::defaults as timeouts;
use sardp::handshake::client_handshake;
use sardp::reason_code::ReasonCode;
use sardp::reconnection::{EstablishOutcome, client_reconnect, establish_connection};
use sardp::session_store::SessionStore;

#[tokio::test]
async fn establish_connection_handles_a_fresh_client_hello() {
    let server = TestServer::start();
    let client_signing_key = SigningKey::from_bytes(&[0x91; 32]);
    let trusted_public_key = client_signing_key.verifying_key();
    let sessions = SessionStore::new();

    let (client_connection, server_connection) = connect_to(&server).await;

    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        establish_connection(
            &server_connection,
            "test-server",
            &trusted_public_key,
            &sessions,
            timeouts::HANDSHAKE_TIMEOUT,
            timeouts::AUTH_TIMEOUT,
        ),
    );
    let (client_outcome, _client_sm, _client_control) =
        client_result.expect("client handshake succeeds");
    let outcome = server_result.expect("establish_connection succeeds");

    let EstablishOutcome::Established(ctx) = outcome else {
        panic!("expected Established, got a ReconnectRejected");
    };
    assert!(!ctx.is_resumed);
    assert_eq!(ctx.starting_generation, 0);
    assert_eq!(ctx.session_id, client_outcome.session_id);
    assert_eq!(ctx.user_id, "alice");
    assert_eq!(ctx.granted_permissions, client_outcome.granted_permissions);
    assert_eq!(
        ctx.connection_sm.state(),
        sardp::connection_sm::ConnectionState::Authenticated
    );
}

#[tokio::test]
async fn establish_connection_resumes_a_suspended_session() {
    let server = TestServer::start();
    let client_signing_key = SigningKey::from_bytes(&[0x92; 32]);
    let trusted_public_key = client_signing_key.verifying_key();
    let sessions = std::sync::Arc::new(SessionStore::new());

    // --- Establish and suspend an original session. ---
    let (client_connection_1, server_connection_1) = connect_to(&server).await;
    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection_1,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        establish_connection(
            &server_connection_1,
            "test-server",
            &trusted_public_key,
            &sessions,
            timeouts::HANDSHAKE_TIMEOUT,
            timeouts::AUTH_TIMEOUT,
        ),
    );
    let (client_outcome, mut client_sm, _client_control_1) = client_result.unwrap();
    let EstablishOutcome::Established(mut ctx) = server_result.unwrap() else {
        panic!("expected a fresh handshake to establish");
    };
    ctx.connection_sm.on_channel_live().unwrap();
    client_sm.on_channel_live().unwrap();

    sessions
        .suspend_and_schedule_expiry(
            ctx.session_id,
            ctx.user_id.clone(),
            ctx.connection_sm,
            ctx.reconnect_token,
            ctx.granted_permissions,
            3,
            std::time::Duration::from_secs(300),
        )
        .expect("Active connection_sm suspends cleanly");
    client_sm.suspend().unwrap();
    drop(client_connection_1);
    drop(server_connection_1);

    // --- Reconnect on a brand new QUIC connection. ---
    let (client_connection_2, server_connection_2) = connect_to(&server).await;

    let (client_reconnect_result, server_result) = tokio::join!(
        client_reconnect(
            &client_connection_2,
            &mut client_sm,
            ctx.session_id,
            ctx.reconnect_token,
            "alice",
        ),
        establish_connection(
            &server_connection_2,
            "test-server",
            &trusted_public_key,
            &sessions,
            timeouts::HANDSHAKE_TIMEOUT,
            timeouts::AUTH_TIMEOUT,
        ),
    );
    let (client_reconnect_outcome, _client_control_2) = client_reconnect_result.unwrap();
    let outcome = server_result.unwrap();

    let EstablishOutcome::Established(resumed_ctx) = outcome else {
        panic!("expected Established, got a ReconnectRejected");
    };
    assert!(resumed_ctx.is_resumed);
    // DR-026: continues from last_generation + 1, not a reset.
    assert_eq!(resumed_ctx.starting_generation, 4);
    assert_eq!(resumed_ctx.session_id, client_outcome.session_id);
    assert_eq!(
        resumed_ctx.reconnect_token,
        client_reconnect_outcome.reconnect_token
    );
    assert_eq!(
        resumed_ctx.connection_sm.state(),
        sardp::connection_sm::ConnectionState::Active
    );
}

#[tokio::test]
async fn establish_connection_reports_a_rejected_reconnect() {
    let server = TestServer::start();
    let client_signing_key = SigningKey::from_bytes(&[0x93; 32]);
    let trusted_public_key = client_signing_key.verifying_key();
    let sessions = SessionStore::new();

    // No session was ever suspended -- any SessionReauthenticate must be
    // rejected. Drive it directly against a raw control stream, mirroring
    // the wire shape `client_reconnect` sends.
    let (client_connection, server_connection) = connect_to(&server).await;

    let mut unknown_sm = sardp::connection_sm::ConnectionSm::new();
    unknown_sm.complete_handshake().unwrap();
    unknown_sm.complete_authentication().unwrap();
    unknown_sm.on_channel_live().unwrap();
    unknown_sm.suspend().unwrap();

    let (client_reconnect_result, server_result) = tokio::join!(
        client_reconnect(
            &client_connection,
            &mut unknown_sm,
            [0xAA; 16],
            [0xBB; 32],
            "nobody",
        ),
        establish_connection(
            &server_connection,
            "test-server",
            &trusted_public_key,
            &sessions,
            timeouts::HANDSHAKE_TIMEOUT,
            timeouts::AUTH_TIMEOUT,
        ),
    );

    assert!(client_reconnect_result.is_err());
    let outcome = server_result.unwrap();
    assert!(matches!(
        outcome,
        EstablishOutcome::ReconnectRejected(ReasonCode::AUTH_RECONNECT_TOKEN_INVALID)
    ));
}
