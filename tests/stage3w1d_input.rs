//! Stage 3 (3W-1-d-1) integration test: the `input` stream (spec 2.12)
//! over real loopback QUIC. Covers wire-level round-tripping only -- this
//! step doesn't decide whether an event should be injected
//! (`permission_sm.is_granted(bit::INPUT_KEYBOARD | bit::INPUT_MOUSE)`,
//! not exercised here) nor how it reaches the OS (SendInput etc., a later
//! 3W-1-d stage, inherently outside this `unsafe_code = "forbid"` crate).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use sardp::input_session::{
    InputMessage, InputReceiver, ReadInputError, open_input_stream, send_ime_composition,
    send_ime_mode_change, send_key_event, send_mouse_button, send_mouse_move, send_text_input,
    send_wheel,
};
use sardp::messages::{
    ImeComposition, ImeMode, ImeModeChange, InputHeader, KeyEvent, MouseButton, MouseMove,
    TextInput, Wheel,
};
use sardp::stream_kind::StreamKind;
use sardp::{net, pki, prologue};

/// Local bind address for the test endpoints. Defaults to 127.0.0.1, but
/// honors `SARDP_TEST_BIND_ADDR` (an IPv4 address) so the test can still
/// run on a machine whose UDP *loopback* is broken while UDP over a real
/// local interface works (KNOWN_ISSUES.md item 14 -- e.g. set it to the
/// machine's LAN address).
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

fn header(event_id: u64) -> InputHeader {
    InputHeader {
        event_id,
        client_ts: 1_000_000,
    }
}

/// `input` (spec 2.2.1: client-initiated, unidirectional client->server).
/// One of each spec 2.12 message type, sent in order on the same stream
/// and read back in the same order.
#[tokio::test]
async fn every_input_message_type_round_trips_over_the_input_stream() {
    let (client, server) = connect_pair().await;

    let mut send = open_input_stream(&client).await.expect("open input stream");

    let key = KeyEvent {
        header: header(1),
        down: true,
        scancode: 0x0004, // USB HID Usage ID for 'A'
        logical_key: 0x61,
        modifiers: 0,
    };
    let text = TextInput {
        header: header(2),
        text: "こんにちは".into(),
    };
    let ime = ImeComposition {
        header: header(3),
        text: "こんにち".into(),
        caret: 4,
    };
    let mouse_move = MouseMove {
        header: header(4),
        x: 640,
        y: 360,
    };
    let mouse_button = MouseButton {
        header: header(5),
        button: 0,
        down: true,
        x: 640,
        y: 360,
    };
    let wheel = Wheel {
        header: header(6),
        dx: 0,
        dy: -120,
        is_precise: false,
    };
    let mode_change = ImeModeChange {
        mode: ImeMode::RemoteSide,
        effective_after_event_id: 6,
    };

    send_key_event(&mut send, &key).await.expect("send key");
    send_text_input(&mut send, &text).await.expect("send text");
    send_ime_composition(&mut send, &ime)
        .await
        .expect("send ime");
    send_mouse_move(&mut send, &mouse_move)
        .await
        .expect("send move");
    send_mouse_button(&mut send, &mouse_button)
        .await
        .expect("send button");
    send_wheel(&mut send, &wheel).await.expect("send wheel");
    send_ime_mode_change(&mut send, &mode_change)
        .await
        .expect("send mode change");

    let mut receiver = InputReceiver::accept(&server)
        .await
        .expect("accept input stream");
    assert_eq!(
        receiver.read_message().await.expect("read key"),
        InputMessage::Key(key)
    );
    assert_eq!(
        receiver.read_message().await.expect("read text"),
        InputMessage::Text(text)
    );
    assert_eq!(
        receiver.read_message().await.expect("read ime"),
        InputMessage::ImeComposition(ime)
    );
    assert_eq!(
        receiver.read_message().await.expect("read move"),
        InputMessage::MouseMove(mouse_move)
    );
    assert_eq!(
        receiver.read_message().await.expect("read button"),
        InputMessage::MouseButton(mouse_button)
    );
    assert_eq!(
        receiver.read_message().await.expect("read wheel"),
        InputMessage::Wheel(wheel)
    );
    assert_eq!(
        receiver.read_message().await.expect("read mode change"),
        InputMessage::ImeModeChange(mode_change)
    );
}

/// Spec 2.2.1's initiation-direction MUST: only the client opens `input`.
/// `InputReceiver::accept` must reject a stream whose `StreamPrologue`
/// claims a different kind (a server implementation would treat this as a
/// protocol violation and disconnect; this test only checks the
/// `input_session` layer's own classification).
#[tokio::test]
async fn accept_rejects_a_non_input_stream_kind() {
    let (client, server) = connect_pair().await;

    let mut send = client.open_uni().await.expect("open uni");
    let mut prologue_bytes = Vec::new();
    prologue::encode(StreamKind::Feedback, 1, 0, &mut prologue_bytes);
    send.write_all(&prologue_bytes)
        .await
        .expect("write prologue");

    let result = InputReceiver::accept(&server).await;
    assert!(matches!(result, Err(ReadInputError::WrongStreamKind)));
}
