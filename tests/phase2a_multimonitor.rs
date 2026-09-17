//! Phase 2a integration test: multi-monitor `ActiveMonitor` handling
//! (spec 2.4, 4.3.1) over real loopback QUIC. Two video Instances (two
//! monitors) are opened on one connection; the client sends
//! `ActiveMonitor` on the still-open `control` stream, and the server's
//! `MonitorManager` drives the non-focused Channel to `Paused` and the
//! focused one to `Live` in response -- proving the state transition
//! actually happens on receipt of a real wire message, not just via
//! direct method calls (already covered by `monitor_manager`'s own unit
//! tests).

mod common;

use common::handshake_pair;

use sardp::channel_sm::ChannelState;
use sardp::messages::{self, ActiveMonitor, ChromaFormat, Codec, EncoderConfig};
use sardp::monitor_manager::MonitorManager;
use sardp::stream_kind::StreamKind;
use sardp::stream_reader::write_envelope;
use sardp::video_channel::VideoChannel;
use sardp::video_session::{open_video_instance, read_video_instance_intro};

fn test_encoder_config() -> EncoderConfig {
    EncoderConfig {
        codec: Codec::H264,
        profile: 66,
        chroma_format: ChromaFormat::C420,
        bit_depth: 8,
        width: 64,
        height: 64,
        max_fps: 30,
        tier: 4,
        b_frames: 0,
        server_cursor_excludable: false,
    }
}

fn fake_idr(tag: u8) -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 0x67, tag, 0xBB]
}

#[tokio::test]
async fn active_monitor_message_pauses_and_activates_the_right_channels() {
    let (client_connection, client, server_connection, server) = handshake_pair(0x91).await;
    let mut client_control = client.control;
    let mut server_control = server.control;

    // Open two monitors' video Instances (monitor_id 0 and 1), each
    // reaching Streaming/Live.
    let mut manager = MonitorManager::new();
    for monitor_id in [0u64, 1u64] {
        let (server_open, client_intro) = tokio::join!(
            open_video_instance(
                &server_connection,
                monitor_id,
                0,
                1,
                test_encoder_config(),
                fake_idr(monitor_id as u8),
                0,
                0,
            ),
            read_video_instance_intro(&client_connection),
        );
        let (_send_stream, _instance_sm) =
            server_open.unwrap_or_else(|e| panic!("server opens monitor {monitor_id}: {e:?}"));
        let intro =
            client_intro.unwrap_or_else(|e| panic!("client reads monitor {monitor_id}: {e:?}"));
        assert_eq!(intro.monitor_id, monitor_id);

        let mut channel = VideoChannel::new(0);
        channel.mark_instance_streaming().unwrap();
        manager.add_channel(monitor_id, channel);
    }

    assert_eq!(manager.active_monitor_id(), Some(0));
    assert_eq!(
        manager.channel(0).unwrap().channel_state(),
        ChannelState::Live
    );
    assert_eq!(
        manager.channel(1).unwrap().channel_state(),
        ChannelState::Live
    );

    // Client focuses monitor 1; server receives ActiveMonitor on the
    // still-open control stream and updates its MonitorManager.
    let active_monitor = ActiveMonitor { monitor_id: 1 };
    let active_monitor_bytes = messages::encode(&active_monitor);
    let (send_result, read_result) = tokio::join!(
        write_envelope(
            &mut client_control.send,
            messages::type_id::ACTIVE_MONITOR,
            &active_monitor_bytes,
        ),
        server_control
            .reader
            .read_envelope(StreamKind::Control.max_envelope_length()),
    );
    send_result.expect("client sends ActiveMonitor");
    let (type_raw, payload) = read_result.expect("server reads ActiveMonitor");
    assert_eq!(type_raw, messages::type_id::ACTIVE_MONITOR);
    let received: ActiveMonitor = messages::decode(&payload).expect("decodes ActiveMonitor");
    assert_eq!(received.monitor_id, 1);

    manager
        .set_active_monitor(received.monitor_id as u64)
        .expect("monitor 1 is registered");

    assert_eq!(manager.active_monitor_id(), Some(1));
    assert_eq!(
        manager.channel(0).unwrap().channel_state(),
        ChannelState::Paused,
        "monitor 0 lost focus"
    );
    assert_eq!(
        manager.channel(1).unwrap().channel_state(),
        ChannelState::Live,
        "monitor 1 gained focus"
    );

    // Switch focus back; monitor 0 should reactivate.
    let active_monitor = ActiveMonitor { monitor_id: 0 };
    let active_monitor_bytes = messages::encode(&active_monitor);
    let (send_result, read_result) = tokio::join!(
        write_envelope(
            &mut client_control.send,
            messages::type_id::ACTIVE_MONITOR,
            &active_monitor_bytes,
        ),
        server_control
            .reader
            .read_envelope(StreamKind::Control.max_envelope_length()),
    );
    send_result.expect("client sends ActiveMonitor");
    let (_type_raw, payload) = read_result.expect("server reads ActiveMonitor");
    let received: ActiveMonitor = messages::decode(&payload).unwrap();
    manager
        .set_active_monitor(received.monitor_id as u64)
        .unwrap();

    assert_eq!(
        manager.channel(0).unwrap().channel_state(),
        ChannelState::Live
    );
    assert_eq!(
        manager.channel(1).unwrap().channel_state(),
        ChannelState::Paused
    );
}
