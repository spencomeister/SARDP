//! DR-037 integration test (spec 2.6: `file_handle`'s "セッション・ユーザー・
//! 方向・有効期限に束縛"): a `file_handle` issued to one session must be
//! rejected when presented on a *different* QUIC connection belonging to a
//! different session/user -- proving that accepting a `file` stream isn't
//! just "does this context_id look like a handle we've issued to anyone",
//! but "was it issued to *this* connection's own session".

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use sardp::file_handle_store::{FileHandleError, FileHandleStore};
use sardp::file_transfer_session::{
    FileTransferSessionError, accept_file_stream_verified,
    accept_file_stream_verified_with_timeout, open_file_stream,
};
use sardp::messages::FileTransferDirection;
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

const TTL: Duration = Duration::from_secs(60);

/// The scenario DR-037 exists for: connection A's session was issued a
/// `file_handle` for its own upload; a second, entirely separate QUIC
/// connection (a different client/session) learns that same `file_handle`
/// value and tries to open the `file` stream for it on its *own*
/// connection. The receiving side must check the handle's actual owner,
/// not just that some session somewhere holds a live handle with that
/// value.
#[tokio::test]
async fn a_file_handle_issued_to_one_session_is_rejected_from_a_different_connection() {
    let (_client_a, _server_a) = connect_pair().await;
    let (client_b, server_b) = connect_pair().await;

    let store = FileHandleStore::new();
    let session_id_a = [0xAA; 16];
    let session_id_b = [0xBB; 16];

    // Issued for session A's upload -- session B never requested this and
    // has no legitimate claim to it.
    let (file_handle, _expiry_ts) = store.issue(
        session_id_a,
        "alice".into(),
        FileTransferDirection::Upload,
        1024,
        TTL,
    );

    let (open_result, accept_result) = tokio::join!(
        open_file_stream(&client_b, file_handle),
        accept_file_stream_verified(
            &server_b,
            &store,
            session_id_b,
            "mallory",
            FileTransferDirection::Upload,
        ),
    );
    // Opening succeeds at the transport level -- `open_file_stream` has no
    // way to know the handle isn't client_b's own; the ownership check is
    // entirely the receiver's job.
    let (_send, _reader) = open_result.expect("client_b can open the stream");

    match accept_result {
        Err(FileTransferSessionError::OwnershipRejected(FileHandleError::SessionMismatch)) => {}
        Err(other) => panic!("expected SessionMismatch, got {other:?}"),
        Ok(_) => panic!("expected the ownership check to reject this handle"),
    }
}

/// Baseline: the actual owning session's own connection is accepted, so the
/// rejection above is proven to be about ownership specifically, not a
/// bug that rejects everything.
#[tokio::test]
async fn the_owning_session_s_own_connection_is_accepted() {
    let (client_a, server_a) = connect_pair().await;

    let store = FileHandleStore::new();
    let session_id_a = [0xAA; 16];
    let (file_handle, _expiry_ts) = store.issue(
        session_id_a,
        "alice".into(),
        FileTransferDirection::Upload,
        1024,
        TTL,
    );

    let (open_result, accept_result) = tokio::join!(
        open_file_stream(&client_a, file_handle),
        accept_file_stream_verified(
            &server_a,
            &store,
            session_id_a,
            "alice",
            FileTransferDirection::Upload,
        ),
    );
    let (_send, _reader) = open_result.expect("client_a opens its own stream");
    let (_send2, _reader2, accepted_handle) =
        accept_result.expect("the owning session's own connection is accepted");
    assert_eq!(accepted_handle, file_handle);
}

/// KNOWN_ISSUES.md #3: a `FileTransferRequest` was accepted (a `file_handle`
/// issued) but the peer never actually opens the promised `file` stream --
/// `accept_file_stream_verified` must give up rather than waiting forever,
/// so the caller (a `spawn`ed task on the server) can log the failure and
/// move on instead of leaking a task per stalled client.
#[tokio::test]
async fn accept_gives_up_once_the_stream_is_never_opened() {
    let (_client_a, server_a) = connect_pair().await;
    let store = FileHandleStore::new();

    let result = accept_file_stream_verified_with_timeout(
        &server_a,
        &store,
        [0xAA; 16],
        "alice",
        FileTransferDirection::Upload,
        Duration::from_millis(50),
    )
    .await;
    assert!(matches!(
        result,
        Err(FileTransferSessionError::AcceptTimeout)
    ));
}
