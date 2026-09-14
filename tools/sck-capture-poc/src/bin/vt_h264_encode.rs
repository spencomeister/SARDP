//! 3M-1-b: ScreenCaptureKit -> VideoToolbox H.264 encode PoC.
//!
//! The macOS counterpart of `tools/dxgi-capture-poc/src/bin/mf_h264_encode.rs`.
//! Captures the main display as NV12 (the format the encoder wants, so
//! there is no colour-conversion step at all -- ScreenCaptureKit does it,
//! where Windows needed an `ID3D11VideoProcessor` pass), encodes it with
//! VideoToolbox, and writes the Annex-B access units to a `.h264` file
//! that `ffprobe`/`ffmpeg` can check.
//!
//! What it verifies, beyond "bytes came out":
//!
//! - The encoder really is the hardware one (read back from the session,
//!   not assumed from the flag we asked for).
//! - Every IDR is self-contained (spec 2.10). VideoToolbox keeps SPS/PPS
//!   *out* of the bitstream, so this only holds because the shim reports
//!   the parameter sets from the format description and
//!   `sardp::h264::ParameterSetCache` prepends them -- the same role
//!   `MF_MT_MPEG_SEQUENCE_HEADER` plays on Windows.
//! - Input/output accounting: how many frames the encoder was given, how
//!   many came back, and how many it dropped on its own.
//!
//! ```text
//! cargo run -p sck-capture-poc --bin vt_h264_encode -- --frames 120
//! ffprobe -v error -show_frames -of csv out.h264 | head
//! ```
//! Run it through `make-app.sh` when the Screen Recording grant matters
//! (see this crate's README): a bare binary is attributed to the terminal.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use sardp::clock::now_us;
use sardp::h264::{self, ParameterSetCache};
use sck_capture_poc::shim::{self, FrameRef, FrameSink, FrameStatus, PixelFormat, Session};
use sck_capture_poc::vt::{EncodedRef, EncodedSink, Encoder};

struct Args {
    frames: usize,
    fps: u32,
    bitrate_bps: u32,
    all_idr: bool,
    idr_every: usize,
    out: PathBuf,
    wait_secs: u64,
}

fn parse_args() -> Args {
    let mut a = Args {
        frames: 60,
        fps: 30,
        bitrate_bps: 8_000_000,
        all_idr: false,
        idr_every: 0,
        out: PathBuf::from("out.h264"),
        wait_secs: 20,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next = |what: &str| it.next().unwrap_or_else(|| panic!("{what} needs a value"));
        match arg.as_str() {
            "--frames" => a.frames = next("--frames").parse().expect("frames"),
            "--fps" => a.fps = next("--fps").parse().expect("fps"),
            "--bitrate" => a.bitrate_bps = next("--bitrate").parse().expect("bitrate"),
            "--all-idr" => a.all_idr = true,
            "--idr-every" => a.idr_every = next("--idr-every").parse().expect("idr-every"),
            "--out" => a.out = PathBuf::from(next("--out")),
            "--wait-secs" => a.wait_secs = next("--wait-secs").parse().expect("secs"),
            "-h" | "--help" => {
                eprintln!(
                    "usage: vt_h264_encode [--frames N] [--fps N] [--bitrate BPS] [--all-idr] \
                     [--idr-every N] [--out FILE] [--wait-secs N]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

fn log(msg: &str) {
    println!("[{:>9}us] {msg}", now_us());
    let _ = std::io::stdout().flush();
}

/// One access unit as the writer thread sees it.
struct Encoded {
    annex_b: Vec<u8>,
    is_idr: bool,
    /// Whether VideoToolbox itself called this a sync sample -- kept only
    /// to show how often the flag and the bytes disagree.
    encoder_said_sync: bool,
    self_contained: bool,
    capture_ts: u64,
    encode_done_ts: u64,
}

/// The VideoToolbox side: runs on a VideoToolbox thread, turns raw output
/// into a self-contained access unit and hands it to `main`.
struct Forwarder {
    tx: SyncSender<Encoded>,
    parameter_sets: ParameterSetCache,
}

impl EncodedSink for Forwarder {
    fn on_encoded(&mut self, frame: EncodedRef<'_>) {
        let encode_done_ts = now_us();
        // VideoToolbox keeps SPS/PPS out of the bitstream entirely, so
        // without this the cache would never fill and every IDR would go
        // out undecodable on its own.
        if let Some(sets) = frame.parameter_sets {
            self.parameter_sets.set_fallback(sets.to_vec());
        }
        let annex_b = frame.annex_b.to_vec();
        // The encoder's flag is an input, not the decision (3W-1-b).
        let is_idr = h264::is_idr_access_unit(&annex_b, frame.is_sync);
        self.parameter_sets.observe(&annex_b);
        let annex_b = if is_idr {
            self.parameter_sets.complete_idr(annex_b)
        } else {
            annex_b
        };
        let self_contained = h264::is_self_contained_idr(&annex_b);
        let _ = self.tx.send(Encoded {
            annex_b,
            is_idr,
            encoder_said_sync: frame.is_sync,
            self_contained,
            capture_ts: frame.capture_ts_us,
            encode_done_ts,
        });
    }
}

/// The ScreenCaptureKit side: runs on SCK's sample-handler queue and feeds
/// the encoder the pixel buffer SCK already produced.
struct CaptureSink {
    encoder: Arc<Encoder>,
    force_idr: Arc<AtomicBool>,
    submitted: u64,
    /// Sample buffers with no new image (SCK's `idle`/`blank`): the macOS
    /// analogue of DXGI's `LastPresentTime == 0`, and the same rule --
    /// never hand one to the encoder.
    no_image: u64,
}

impl FrameSink for CaptureSink {
    fn on_frame(&mut self, frame: FrameRef<'_>) {
        let Some(pixel_buffer) = frame
            .pixel_buffer
            .filter(|_| matches!(frame.status, FrameStatus::Complete | FrameStatus::Started))
        else {
            self.no_image += 1;
            return;
        };
        let capture_ts = now_us();
        let force_idr = self.force_idr.swap(false, Ordering::SeqCst);
        if let Err(e) = self.encoder.encode(pixel_buffer, capture_ts, force_idr) {
            eprintln!("[vt] encode refused a frame: {e}");
            return;
        }
        self.submitted += 1;
    }

    fn on_stopped(&mut self, message: &str) {
        eprintln!("[sck] stream stopped: {message}");
    }
}

fn main() {
    let args = parse_args();

    if !shim::preflight_screen_capture_access() {
        eprintln!(
            "screen recording permission is not granted for this process. Run through \
             make-app.sh and tick the app in System Settings > Privacy & Security > \
             Screen & System Audio Recording (no dialog is shown; see README)."
        );
        std::process::exit(3);
    }
    let display = shim::main_display_info().expect("main display");
    let fps = args.fps.min(display.refresh_hz.round().max(1.0) as u32);
    log(&format!(
        "display {}x{}px origin=({}, {}) refresh={:.0}Hz -> encoding at {}fps, {} bps{}",
        display.pixels_w,
        display.pixels_h,
        display.origin_x,
        display.origin_y,
        display.refresh_hz,
        fps,
        args.bitrate_bps,
        if args.all_idr { ", all-IDR" } else { "" }
    ));

    let (tx, rx): (SyncSender<Encoded>, Receiver<Encoded>) = sync_channel(16);
    let encoder = Arc::new(
        Encoder::new(
            display.pixels_w,
            display.pixels_h,
            fps,
            args.bitrate_bps,
            args.all_idr,
            Forwarder {
                tx,
                parameter_sets: ParameterSetCache::new(),
            },
        )
        .expect("create VideoToolbox encoder"),
    );
    log(&format!(
        "VideoToolbox session ready; hardware encoder in use: {}",
        encoder.uses_hardware()
    ));

    let force_idr = Arc::new(AtomicBool::new(false));
    let session = Session::start(
        fps,
        PixelFormat::Nv12,
        false,
        15_000,
        false,
        CaptureSink {
            encoder: encoder.clone(),
            force_idr: force_idr.clone(),
            submitted: 0,
            no_image: 0,
        },
    )
    .expect("start ScreenCaptureKit capture");
    log("capture started");

    let mut out = BufWriter::new(File::create(&args.out).expect("create output file"));
    let mut written = 0usize;
    let mut bytes = 0usize;
    let mut idr_count = 0usize;
    let mut not_self_contained = 0usize;
    let mut flag_disagreed = 0usize;
    let mut encode_us: Vec<u64> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(args.wait_secs);

    while written < args.frames && Instant::now() < deadline {
        let Ok(frame) = rx.recv_timeout(Duration::from_secs(5)) else {
            log("no encoded frame in 5s (a completely static screen produces none)");
            continue;
        };
        if frame.is_idr {
            idr_count += 1;
            if !frame.self_contained {
                not_self_contained += 1;
            }
        }
        if frame.is_idr != frame.encoder_said_sync {
            flag_disagreed += 1;
        }
        let latency = frame.encode_done_ts.saturating_sub(frame.capture_ts);
        encode_us.push(latency);
        out.write_all(&frame.annex_b).expect("write");
        bytes += frame.annex_b.len();
        written += 1;
        if written <= 5 || written.is_multiple_of(30) {
            log(&format!(
                "frame {written}: {} bytes, idr={} self_contained={} capture->encode={}us nal_types={:?}",
                frame.annex_b.len(),
                frame.is_idr,
                frame.self_contained,
                latency,
                h264::nal_unit_types(&frame.annex_b),
            ));
        }
        if args.idr_every > 0 && written.is_multiple_of(args.idr_every) {
            force_idr.store(true, Ordering::SeqCst);
        }
    }

    // Teardown order matters: stop the capture first so no new frame can
    // be submitted, then flush what the encoder is still holding.
    drop(session);
    encoder.complete();
    while let Ok(frame) = rx.try_recv() {
        out.write_all(&frame.annex_b).expect("write");
        bytes += frame.annex_b.len();
        written += 1;
    }
    out.flush().expect("flush");

    let stats = encoder.stats();
    encode_us.sort_unstable();
    let pct = |p: usize| {
        encode_us
            .get((encode_us.len().saturating_sub(1)) * p / 100)
            .copied()
            .unwrap_or(0)
    };
    log(&format!(
        "wrote {} frames ({} bytes) to {}",
        written,
        bytes,
        args.out.display()
    ));
    log(&format!(
        "encoder: inputs={} outputs={} dropped_by_encoder={} callback_errors={} hardware={}",
        stats.inputs,
        stats.outputs,
        stats.dropped_by_encoder,
        stats.callback_errors,
        encoder.uses_hardware()
    ));
    log(&format!(
        "capture->encode-done: p50={}us p95={}us max={}us",
        pct(50),
        pct(95),
        encode_us.last().copied().unwrap_or(0)
    ));
    log(&format!(
        "IDRs: {idr_count} (not self-contained: {not_self_contained}); \
         frames where the encoder's sync flag disagreed with the NAL units: {flag_disagreed}"
    ));
    if not_self_contained > 0 {
        eprintln!("FAIL: {not_self_contained} IDR(s) lacked SPS/PPS (spec 2.10)");
        std::process::exit(1);
    }
    if written == 0 {
        eprintln!("FAIL: no frames encoded");
        std::process::exit(1);
    }
}
