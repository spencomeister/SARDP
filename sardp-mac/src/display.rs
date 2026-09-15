//! H.264 decode + on-screen display as a *sink* (3M-1-e): the macOS
//! mirror of `sardp_win::display::H264DisplayWindow`, deliberately the
//! same shape (`open`/`submit`/`next_timing`/`is_closed`,
//! `SubmittedFrame`/`FrameTiming`/`DisplayConfig`) so `sardp-cli` wires
//! both platforms through one `os::H264DisplayWindow` alias
//! (KNOWN_ISSUES #28) exactly as it already does for capture and input.
//!
//! [`H264DisplayWindow::open`] spawns a dedicated OS thread that owns an
//! on-screen window (`sck_capture_poc::window::DisplayWindow`, an AppKit
//! `NSWindow`) and a persistent VideoToolbox H.264 decoder
//! (`sck_capture_poc::vtdec::Decoder`) for their whole lifetime. Each
//! decoded frame's `CVPixelBuffer` is IOSurface-backed and handed to the
//! window's `CALayer` directly -- no CPU-side pixel copy, the same
//! property DXVA + `ID3D11VideoProcessor` gets on Windows, just reached
//! by a different pair of APIs.
//!
//! The decoder is persistent for the window's lifetime (DR-036: no
//! per-frame process spawn), which is also what makes P-frames decodable
//! at all -- the earlier per-frame `ffmpeg` decoder (`--display log`)
//! could only ever handle self-contained IDRs.
//!
//! Where this genuinely differs from the Windows path, beyond the OS
//! APIs themselves:
//!
//! - **VideoToolbox needs a format description before it will decode
//!   anything**, built from SPS/PPS (`CMVideoFormatDescriptionCreateFromH264ParameterSets`,
//!   in `VtDecoder.swift`). Media Foundation's decoder MFT parses SPS/PPS
//!   out of the Annex-B bitstream itself, so Windows never has to go
//!   looking for them; here [`split_for_decode`] does, using
//!   `sardp::h264::split_annex_b` -- the same module that already keeps
//!   every IDR self-contained on the server side, so every IDR this
//!   worker sees is guaranteed (by that code, spec 2.10) to carry them.
//! - **No swap-chain backpressure to poll for.** Windows' `Presenter`
//!   checks a frame-latency waitable object and may decode a frame
//!   without presenting it (the swap chain still held the previous one).
//!   Setting a `CALayer`'s `contents` has no equivalent blocking point at
//!   this level, so every successfully decoded frame is presented; this
//!   worker's `FrameTiming.presented` is `false` only when decode itself
//!   produced no output (nothing to show), not from throttling.
//! - **AppKit runs on this worker thread, not the process's real main
//!   thread**, which needed its own verification before anything else in
//!   this file was written -- see `DisplayWindow.swift`'s doc comment
//!   for what had to be true (and how it was checked, not assumed) for a
//!   window created off the "real" main thread to actually appear.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use sardp::frame_source::Clock;
use sardp::h264;
use sardp::worker_handle::WorkerHandle;
use sck_capture_poc::vtdec::Decoder;
use sck_capture_poc::window::DisplayWindow;

#[derive(Debug, Clone)]
pub struct DisplayConfig {
    pub title: String,
    /// Client-area size of the window, in points. Frames are scaled to
    /// fit (the layer's `contentsGravity`), so this is independent of
    /// the stream's own resolution -- same contract as
    /// `sardp_win::DisplayConfig`.
    pub width: u32,
    pub height: u32,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            title: "SARDP".into(),
            width: 1280,
            height: 720,
        }
    }
}

/// One H.264 access unit handed to the display, with the wire-level
/// identity the timing report echoes back. Identical shape to
/// `sardp_win::display::SubmittedFrame`.
#[derive(Debug)]
pub struct SubmittedFrame {
    pub generation: u64,
    pub frame_id: u64,
    pub is_idr: bool,
    pub width: u32,
    pub height: u32,
    pub annex_b: Vec<u8>,
    /// Client clock when the frame's bytes finished arriving.
    pub receive_ts: u64,
}

/// Per-frame timestamps from the display thread (client clock, same
/// basis as `receive_ts`), for `TransportFeedback` (spec 2.14). Identical
/// shape to `sardp_win::display::FrameTiming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    pub generation: u64,
    pub frame_id: u64,
    pub receive_ts: u64,
    pub dequeue_ts: u64,
    pub decode_done_ts: u64,
    pub display_ts: u64,
    /// False when decode produced no output for this call (should not
    /// happen for this protocol's B-frame-free streams, but nothing here
    /// assumes it can't); unlike Windows, never false due to display
    /// throttling -- see the module doc.
    pub presented: bool,
}

#[derive(Debug)]
pub enum MacDisplayError {
    Init(String),
    /// The window was closed (by the user) or its thread died.
    Closed,
}

impl std::fmt::Display for MacDisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(s) => write!(f, "display init failed: {s}"),
            Self::Closed => write!(f, "display window closed"),
        }
    }
}

impl std::error::Error for MacDisplayError {}

/// Shared between the submitting side and the display thread. Same role
/// (and same field shape) as `sardp_win::display`'s private `Shared`.
struct Shared {
    closed: AtomicBool,
    /// Highest generation submitted so far; the display thread skips
    /// queued frames older than this once the server has reset and
    /// reopened (spec 2.10) rather than spend decode time on a backlog
    /// that is about to be superseded anyway.
    newest_generation: AtomicU64,
}

pub struct H264DisplayWindow {
    /// Unbounded, like the Windows counterpart: dropping a frame here
    /// would corrupt the reference chain of a P-frame stream with no way
    /// for this client to ask for a new IDR on its own. A backlog instead
    /// shows up as growing `client_queue_delay_us` in `TransportFeedback`,
    /// and the server's own backpressure (spec 2.10) or this client's
    /// circuit breaker (KNOWN_ISSUES #16/#29) are what resolve it.
    handle: WorkerHandle<SubmittedFrame>,
    timing_rx: mpsc::Receiver<FrameTiming>,
    shared: Arc<Shared>,
}

impl H264DisplayWindow {
    /// Creates the window and the (not-yet-configured) decoder on their
    /// own thread and returns once the window has actually confirmed
    /// itself visible to the window server (not merely that creating it
    /// didn't error -- see the module doc).
    pub fn open(config: DisplayConfig, clock: Clock) -> Result<Self, MacDisplayError> {
        let (tx, rx) = std::sync::mpsc::channel::<SubmittedFrame>();
        let (timing_tx, timing_rx) = mpsc::channel::<FrameTiming>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let shared = Arc::new(Shared {
            closed: AtomicBool::new(false),
            newest_generation: AtomicU64::new(0),
        });

        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("sardp-mac-display".into())
                .spawn(move || worker_main(config, clock, rx, timing_tx, ready_tx, shared))
                .map_err(|e| MacDisplayError::Init(format!("spawn display thread: {e}")))?
        };

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(MacDisplayError::Init(e));
            }
            Err(_) => {
                return Err(MacDisplayError::Init(
                    "display thread did not become ready within 15s".into(),
                ));
            }
        }

        Ok(Self {
            handle: WorkerHandle::new(tx, worker),
            timing_rx,
            shared,
        })
    }

    /// Queues a frame for decode+display. Never blocks and never drops
    /// (see the `handle` field's doc); frames of a generation older than
    /// the newest submitted one are skipped by the display thread.
    pub fn submit(&self, frame: SubmittedFrame) -> Result<(), MacDisplayError> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(MacDisplayError::Closed);
        }
        self.shared
            .newest_generation
            .fetch_max(frame.generation, Ordering::SeqCst);
        self.handle.send(frame).map_err(|_| MacDisplayError::Closed)
    }

    /// Next decode/present timing report; `None` once the window has
    /// been closed (by the user) and its thread has exited.
    pub async fn next_timing(&mut self) -> Option<FrameTiming> {
        self.timing_rx.recv().await
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

impl Drop for H264DisplayWindow {
    fn drop(&mut self) {
        // Set first: a manual `Drop::drop` body always runs before a
        // struct's fields auto-drop, so the worker's poll loop already
        // sees `closed` by the time `handle`'s own drop (right after this
        // method returns) closes the channel and joins.
        self.shared.closed.store(true, Ordering::SeqCst);
    }
}

fn worker_main(
    config: DisplayConfig,
    clock: Clock,
    rx: Receiver<SubmittedFrame>,
    timing_tx: mpsc::Sender<FrameTiming>,
    ready_tx: Sender<Result<(), String>>,
    shared: Arc<Shared>,
) {
    let mut ready_tx = Some(ready_tx);
    if let Err(e) = run_worker(config, &clock, &rx, &timing_tx, &mut ready_tx, &shared) {
        if let Some(ready) = ready_tx.take() {
            let _ = ready.send(Err(e));
        } else {
            eprintln!("[sardp-mac] display thread stopped: {e}");
        }
    }
    shared.closed.store(true, Ordering::SeqCst);
}

/// How long a newly created window may take to confirm itself visible
/// before this is treated as an init failure (rather than trusting
/// `DisplayWindow::create` not panicking -- see the module doc on why
/// that alone would not be enough).
const WINDOW_VISIBLE_TIMEOUT: Duration = Duration::from_secs(5);

fn run_worker(
    config: DisplayConfig,
    clock: &Clock,
    rx: &Receiver<SubmittedFrame>,
    timing_tx: &mpsc::Sender<FrameTiming>,
    ready_tx: &mut Option<Sender<Result<(), String>>>,
    shared: &Shared,
) -> Result<(), String> {
    let window = DisplayWindow::create(config.width, config.height, &config.title);
    let deadline = Instant::now() + WINDOW_VISIBLE_TIMEOUT;
    loop {
        window.pump();
        if window.is_visible() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "window did not become visible within {WINDOW_VISIBLE_TIMEOUT:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if let Some(ready) = ready_tx.take() {
        let _ = ready.send(Ok(()));
    }

    let mut decoder = Decoder::new();
    let mut decoder_dims: Option<(u32, u32)> = None;

    loop {
        window.pump();
        if shared.closed.load(Ordering::SeqCst) || window.should_close() {
            return Ok(());
        }
        let frame = match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                // The client dropped the handle.
                return Ok(());
            }
        };
        if frame.generation < shared.newest_generation.load(Ordering::SeqCst) {
            continue;
        }

        let dequeue_ts = clock();
        match decode_and_present(
            &mut decoder,
            &mut decoder_dims,
            &window,
            frame,
            dequeue_ts,
            clock,
        ) {
            Ok(Some(timing)) => {
                if timing_tx.blocking_send(timing).is_err() {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Decodes one frame and presents whatever comes out. `Ok(None)` means
/// nothing to report yet (an IDR arrived before the decoder could be
/// configured -- logged, not fatal; a genuinely malformed protocol
/// stream can only recover on the next self-contained IDR anyway, same
/// as any other decode-error recovery in this protocol, spec 2.10).
fn decode_and_present(
    decoder: &mut Decoder,
    decoder_dims: &mut Option<(u32, u32)>,
    window: &DisplayWindow,
    frame: SubmittedFrame,
    dequeue_ts: u64,
    clock: &Clock,
) -> Result<Option<FrameTiming>, String> {
    let (params, avcc) = split_for_decode(&frame.annex_b);
    if frame.is_idr {
        match params {
            Some((sps, pps)) => {
                let needs_new_decoder = *decoder_dims != Some((frame.width, frame.height));
                if needs_new_decoder {
                    if decoder.is_configured() {
                        eprintln!(
                            "[sardp-mac] stream size changed to {}x{}, recreating decoder",
                            frame.width, frame.height
                        );
                    }
                    decoder
                        .configure(frame.width, frame.height, &sps, &pps)
                        .map_err(|e| e.to_string())?;
                    *decoder_dims = Some((frame.width, frame.height));
                }
            }
            None => {
                // spec 2.10 requires every IDR to be self-contained; the
                // server's `sardp::h264::ParameterSetCache` guarantees
                // this in practice (KNOWN_ISSUES #20's macOS encoder path
                // uses it too), so seeing one without SPS/PPS means a
                // protocol violation upstream. Not fatal to this worker:
                // there is nothing useful to decode from this frame
                // either way, and the stream recovers the same way any
                // other decode gap does, on the next self-contained IDR.
                eprintln!(
                    "[sardp-mac] IDR frame generation={} frame_id={} has no SPS/PPS \
                     (not self-contained, spec 2.10); skipping",
                    frame.generation, frame.frame_id
                );
                return Ok(None);
            }
        }
    }
    if !decoder.is_configured() {
        // A P-frame (or an IDR the branch above already logged and
        // skipped) arrived before any IDR could configure the decoder.
        return Ok(None);
    }

    let output = decoder
        .decode(&avcc, frame.receive_ts)
        .map_err(|e| e.to_string())?;
    let decode_done_ts = clock();
    let presented = match output {
        Some(pixel_buffer) => {
            let shown = window.present(pixel_buffer);
            if !shown {
                eprintln!("[sardp-mac] decoded frame was not IOSurface-backed; could not present");
            }
            shown
        }
        None => false,
    };
    let display_ts = clock();
    Ok(Some(FrameTiming {
        generation: frame.generation,
        frame_id: frame.frame_id,
        receive_ts: frame.receive_ts,
        dequeue_ts,
        decode_done_ts,
        display_ts,
        presented,
    }))
}

/// Splits an access unit into its SPS+PPS (if this AU carried them -- raw
/// NAL bytes, start codes excluded, ready for
/// `Decoder::configure`) and the AVCC-encoded (4-byte big-endian length
/// prefixes) remainder: every other NAL unit, ready for
/// `Decoder::decode`. VideoToolbox already has the parameter sets from
/// `configure`, so they are deliberately not repeated in the AVCC output
/// (see `VtDecoder.swift`'s doc comment on why that would be redundant
/// at best).
fn split_for_decode(annex_b: &[u8]) -> (Option<(Vec<u8>, Vec<u8>)>, Vec<u8>) {
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut avcc = Vec::with_capacity(annex_b.len());
    for unit in h264::split_annex_b(annex_b) {
        match unit.nal_type {
            h264::NAL_TYPE_SPS => {
                sps.get_or_insert_with(|| unit.bytes.to_vec());
            }
            h264::NAL_TYPE_PPS => {
                pps.get_or_insert_with(|| unit.bytes.to_vec());
            }
            _ => {
                let len = u32::try_from(unit.bytes.len()).unwrap_or(u32::MAX);
                avcc.extend_from_slice(&len.to_be_bytes());
                avcc.extend_from_slice(unit.bytes);
            }
        }
    }
    (sps.zip(pps), avcc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(nal_type: u8, extra: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.push(nal_type);
        v.extend_from_slice(extra);
        v
    }

    #[test]
    fn separates_parameter_sets_from_avcc_slice_data() {
        let mut buf = nal(h264::NAL_TYPE_SPS, &[0xAA]);
        buf.extend(nal(h264::NAL_TYPE_PPS, &[0xBB]));
        buf.extend(nal(h264::NAL_TYPE_SLICE_IDR, &[0xCC, 0xDD]));
        let (params, avcc) = split_for_decode(&buf);
        let (sps, pps) = params.expect("both present");
        assert_eq!(sps, vec![h264::NAL_TYPE_SPS, 0xAA]);
        assert_eq!(pps, vec![h264::NAL_TYPE_PPS, 0xBB]);
        // 4-byte big-endian length (3) + the IDR NAL's 3 bytes, and only
        // that -- the parameter sets are not repeated in the AVCC output.
        assert_eq!(avcc, vec![0, 0, 0, 3, h264::NAL_TYPE_SLICE_IDR, 0xCC, 0xDD]);
    }

    #[test]
    fn a_p_frame_without_parameter_sets_yields_none_and_the_slice_data_alone() {
        let buf = nal(1, &[0xEE]); // non-IDR slice, nal_type 1
        let (params, avcc) = split_for_decode(&buf);
        assert!(params.is_none());
        assert_eq!(avcc, vec![0, 0, 0, 2, 1, 0xEE]);
    }

    #[test]
    fn multiple_avcc_nal_units_are_each_individually_length_prefixed() {
        let mut buf = nal(6, &[0x01]); // SEI, arbitrary non-parameter-set type
        buf.extend(nal(h264::NAL_TYPE_SLICE_IDR, &[0x02, 0x03]));
        let (_, avcc) = split_for_decode(&buf);
        assert_eq!(
            avcc,
            vec![
                0,
                0,
                0,
                2,
                6,
                0x01, // SEI: length 2
                0,
                0,
                0,
                3,
                h264::NAL_TYPE_SLICE_IDR,
                0x02,
                0x03, // IDR: length 3
            ]
        );
    }
}
