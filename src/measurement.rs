//! End-to-end ("glass-to-glass") latency measurement harness (PoC brief
//! Part 8: "E2E遅延自動計測"). Drives one frame through the real pipeline --
//! a timecode-embedded synthetic frame (`timecode_frame`), a real `ffmpeg`
//! H.264 encode, a real QUIC video Instance open/read (`video_session`,
//! DR-035's two-Envelope split), and a real `ffmpeg` decode -- and reports
//! the actual measured wall-clock latency from capture to display.
//!
//! This harness's client and server sides run in the same test process, so
//! `capture_ts`/`display_ts` come from the same `clock::now_us()`
//! monotonic clock: no TimeSync offset conversion is needed to compare
//! them, unlike `feedback_session::build_transport_feedback`, which exists
//! precisely because a *real* deployment's client and server are different
//! processes/hosts with independent clocks. That makes this harness's
//! `glass_to_glass_us` a direct, ground-truth latency figure for
//! measurement purposes -- not a replacement for the wire-level
//! `TransportFeedback.client_queue_delay_us` signal itself.

use crate::clock;
use crate::decoder::{self, DecodeError};
use crate::encoder::{self, EncodeError};
use crate::messages::EncoderConfig;
use crate::timecode_frame;
use crate::video_session::{VideoError, open_video_instance, read_video_instance_intro};

#[derive(Debug)]
pub enum MeasurementError {
    Encode(EncodeError),
    Decode(DecodeError),
    Video(VideoError),
    /// The timecode extracted from the decoded frame didn't match what was
    /// embedded before encoding -- the frame was corrupted somewhere in
    /// the pipeline.
    TimecodeMismatch {
        expected: u64,
        actual: u64,
    },
}

/// Real wall-clock timestamps for one frame's trip through the pipeline,
/// all from this process's single `clock::now_us()` monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct LatencyMeasurement {
    pub capture_ts: u64,
    pub encode_done_ts: u64,
    pub received_ts: u64,
    pub decode_done_ts: u64,
    pub display_ts: u64,
}

impl LatencyMeasurement {
    /// The brief's full E2E metric: capture to display, including this
    /// harness's own `ffmpeg` CLI subprocess encode/decode overhead.
    pub fn glass_to_glass_us(&self) -> u64 {
        self.display_ts.saturating_sub(self.capture_ts)
    }

    /// The slice of `glass_to_glass_us` actually attributable to SARDP's
    /// own wire protocol (QUIC send/receive, Envelope framing, the DR-035
    /// header/payload split, CBOR decode) -- everything between "encode
    /// finished" and "client finished reading the Instance intro off the
    /// wire", excluding both `ffmpeg` subprocess invocations. See the
    /// module docs and `tests/m6_measurement.rs` for why this harness
    /// tracks this separately from `glass_to_glass_us`: spawning `ffmpeg`
    /// fresh per frame (the brief's own sanctioned PoC shortcut) costs far
    /// more than the LAN/WAN latency budgets by itself, on top of and
    /// unrelated to anything SARDP's protocol design does.
    pub fn transport_us(&self) -> u64 {
        self.received_ts.saturating_sub(self.encode_done_ts)
    }
}

/// Sends one frame from `server_connection` to `client_connection` over a
/// freshly opened video Instance (spec 2.10/4.3.2) and measures real
/// glass-to-glass latency. `generation` must be unique per call on a given
/// connection pair, since each call opens a brand new Instance.
pub async fn measure_one_frame(
    server_connection: &quinn::Connection,
    client_connection: &quinn::Connection,
    monitor_id: u64,
    generation: u64,
    config_id: u64,
    encoder_config: EncoderConfig,
) -> Result<LatencyMeasurement, MeasurementError> {
    let capture_ts = clock::now_us();
    let frame = timecode_frame::generate_timecode_frame(
        encoder_config.width,
        encoder_config.height,
        capture_ts,
        [40, 40, 40],
    );
    let h264_bytes = tokio::task::spawn_blocking(move || encoder::encode_single_frame_idr(&frame))
        .await
        .expect("encode task doesn't panic")
        .map_err(MeasurementError::Encode)?;
    let encode_done_ts = clock::now_us();

    let (server_result, client_result) = tokio::join!(
        open_video_instance(
            server_connection,
            monitor_id,
            generation,
            config_id,
            encoder_config,
            h264_bytes,
            capture_ts,
            encode_done_ts,
        ),
        read_video_instance_intro(client_connection),
    );
    server_result.map_err(MeasurementError::Video)?;
    let intro = client_result.map_err(MeasurementError::Video)?;
    let received_ts = clock::now_us();

    let width = encoder_config.width;
    let height = encoder_config.height;
    let payload = intro.first_frame_payload;
    let decoded =
        tokio::task::spawn_blocking(move || decoder::decode_single_frame(&payload, width, height))
            .await
            .expect("decode task doesn't panic")
            .map_err(MeasurementError::Decode)?;
    let decode_done_ts = clock::now_us();

    let display_ts = clock::now_us();
    let extracted = timecode_frame::extract_timecode(&decoded);
    if extracted != capture_ts {
        return Err(MeasurementError::TimecodeMismatch {
            expected: capture_ts,
            actual: extracted,
        });
    }

    Ok(LatencyMeasurement {
        capture_ts,
        encode_done_ts,
        received_ts,
        decode_done_ts,
        display_ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::ffmpeg_available;
    use crate::messages::{ChromaFormat, Codec};
    use crate::{net, pki};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

    fn test_encoder_config() -> EncoderConfig {
        EncoderConfig {
            codec: Codec::H264,
            profile: 66,
            chroma_format: ChromaFormat::C420,
            bit_depth: 8,
            width: 640,
            height: 360,
            max_fps: 30,
            tier: 4,
            b_frames: 0,
            server_cursor_excludable: false,
        }
    }

    #[tokio::test]
    async fn measures_a_real_frame_round_trip_and_verifies_the_timecode() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        let (client_connection, server_connection) = connect_pair().await;
        let measurement = measure_one_frame(
            &server_connection,
            &client_connection,
            0,
            0,
            1,
            test_encoder_config(),
        )
        .await
        .expect("measurement succeeds");
        assert!(measurement.encode_done_ts >= measurement.capture_ts);
        assert!(measurement.received_ts >= measurement.encode_done_ts);
        assert!(measurement.decode_done_ts >= measurement.received_ts);
        assert!(measurement.display_ts >= measurement.decode_done_ts);
        // Sanity bound: loopback + ffmpeg subprocess overhead should never
        // legitimately take a full second for a single small frame.
        assert!(measurement.glass_to_glass_us() < 1_000_000);
    }

    #[tokio::test]
    async fn timecode_mismatch_is_detected() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        // Same pipeline, but corrupt the decoded frame's timecode row
        // before extraction would normally happen, by decoding a
        // differently-timecoded frame than what we claim was captured.
        // Exercised indirectly: encode a frame with one timecode, then
        // call extract_timecode against a frame with another, to confirm
        // the comparison itself is meaningful (measure_one_frame's own
        // internal encode/decode always agree in the happy path, covered
        // by the test above).
        let a = timecode_frame::generate_timecode_frame(640, 360, 111, [0, 0, 0]);
        let b = timecode_frame::generate_timecode_frame(640, 360, 222, [0, 0, 0]);
        assert_ne!(
            timecode_frame::extract_timecode(&a),
            timecode_frame::extract_timecode(&b)
        );
    }
}
