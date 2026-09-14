//! Stage 3 (3W-1-d-1) integration test: the `input` stream (spec 2.12)
//! over real loopback QUIC. Covers wire-level round-tripping only -- this
//! step doesn't decide whether an event should be injected
//! (`permission_sm.is_granted(bit::INPUT_KEYBOARD | bit::INPUT_MOUSE)`,
//! not exercised here) nor how it reaches the OS (SendInput etc., a later
//! 3W-1-d stage, inherently outside this `unsafe_code = "forbid"` crate).

mod common;

use common::connect_pair;

use sardp::input_session::{
    InputMessage, InputReceiver, ReadInputError, open_input_stream, send_ime_composition,
    send_ime_mode_change, send_key_event, send_mouse_button, send_mouse_move, send_text_input,
    send_wheel,
};
use sardp::messages::{
    ImeComposition, ImeMode, ImeModeChange, InputHeader, KeyEvent, MouseButton, MouseMove,
    TextInput, Wheel,
};
use sardp::prologue;
use sardp::stream_kind::StreamKind;

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
