//! Platform-independent half of a real-desktop video source: the types a
//! capture+encode pipeline hands to `sardp-server`, and the worker-thread
//! pattern every platform's pipeline runs inside.
//!
//! The OS-specific pipelines (`sardp-win::DesktopH264Source` today; macOS
//! and Linux to follow) own a thread that blocks on OS capture APIs and a
//! hardware encoder. What's common -- and what this module provides so
//! each platform only writes the OS part -- is:
//!
//! - [`EncodedFrame`] / [`SourceInfo`] / [`DesktopH264Config`]: what comes
//!   out, what the stream looks like, how to encode.
//! - [`FrameWorker`]: spawn the thread, wait (bounded) for it to report
//!   [`SourceInfo`], receive frames asynchronously, stop + join on drop.
//! - [`FrameSender`]: the bounded channel with **source-side drop**
//!   (DR-007): when the consumer falls behind, the newest frames are
//!   dropped at the source rather than queued, so latency never grows
//!   unbounded on the sending side. Drops are counted and logged at
//!   powers of two.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

/// Monotonic microsecond clock a pipeline stamps `capture_ts` /
/// `encode_done_ts` with (`sardp-server` passes `clock::now_us`).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Encoder settings for a desktop H.264 source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopH264Config {
    pub bitrate_bps: u32,
    /// Encode every frame as an IDR (GOP size 1). Costs bitrate but keeps
    /// each frame independently decodable -- needed for a client that
    /// decodes each frame with a fresh `ffmpeg` process (`sardp-client
    /// --display log`). Off: IDR on request (at each generation open) plus
    /// a long safety-net GOP; the rest are P-frames.
    pub all_idr: bool,
    /// How long one capture wait may block before the worker re-checks
    /// its stop flag (Windows: the `AcquireNextFrame` timeout).
    pub acquire_timeout: Duration,
}

impl Default for DesktopH264Config {
    fn default() -> Self {
        Self {
            bitrate_bps: 8_000_000,
            all_idr: false,
            acquire_timeout: Duration::from_millis(500),
        }
    }
}

/// What a source learned about the display once its first frame arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceInfo {
    pub width: u32,
    pub height: u32,
    /// Display refresh rate, rounded; also the encoder's nominal frame rate.
    pub fps: u32,
    /// Top-left of the captured output in virtual-desktop coordinates:
    /// input positions relative to the captured image are offset by this
    /// before injection.
    pub origin_x: i32,
    pub origin_y: i32,
}

/// One encoded frame: an Annex-B H.264 access unit. When `is_idr`, the
/// bytes are self-contained (SPS+PPS precede the IDR slice, spec 2.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub annex_b: Vec<u8>,
    pub is_idr: bool,
    pub capture_ts: u64,
    pub encode_done_ts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The worker thread failed to set up capture/encode (or never
    /// reported readiness in time).
    Init(String),
    /// The worker thread died after startup.
    Worker(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(s) => write!(f, "desktop capture init failed: {s}"),
            Self::Worker(s) => write!(f, "desktop capture worker failed: {s}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// How many frames may wait for the consumer before the source starts
/// dropping (DR-007). Small on purpose: a queue here is latency.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 4;

/// What happened to a frame handed to [`FrameSender::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The consumer is behind and the channel is full: dropped at the
    /// source (DR-007), counted, never blocks the producer.
    Dropped,
    /// The consumer is gone; the producer should stop.
    Closed,
}

/// The producer end of a bounded frame channel with source-side drop.
pub struct FrameSender<T> {
    tx: mpsc::Sender<T>,
    dropped: u64,
    label: String,
}

impl<T> FrameSender<T> {
    pub fn new(tx: mpsc::Sender<T>, label: impl Into<String>) -> Self {
        Self {
            tx,
            dropped: 0,
            label: label.into(),
        }
    }

    /// Never blocks.
    pub fn send(&mut self, item: T) -> SendOutcome {
        match self.tx.try_send(item) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped += 1;
                if self.dropped.is_power_of_two() {
                    eprintln!(
                        "[{}] consumer behind; dropped {} frame(s) so far",
                        self.label, self.dropped
                    );
                }
                SendOutcome::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    /// Frames dropped at the source so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// What a worker body gets: where frames go, when to stop, and how to
/// report readiness.
pub struct WorkerContext<T, I> {
    pub frames: FrameSender<T>,
    stop: Arc<AtomicBool>,
    ready: Option<std::sync::mpsc::Sender<Result<I, String>>>,
}

impl<T, I> WorkerContext<T, I> {
    /// Set once the owning [`FrameWorker`] is dropped; a body should poll
    /// this at least as often as its capture timeout.
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Reports the stream parameters to whoever is waiting in
    /// [`FrameWorker::spawn`]. Only the first call counts; returns whether
    /// this call was it.
    pub fn report_ready(&mut self, info: I) -> bool {
        match self.ready.take() {
            Some(ready) => {
                let _ = ready.send(Ok(info));
                true
            }
            None => false,
        }
    }

    /// Whether readiness has already been reported.
    pub fn is_ready(&self) -> bool {
        self.ready.is_none()
    }
}

/// A running source: a worker thread producing `T` frames, which reported
/// `I` once ready. Dropping it requests a stop and joins the thread.
pub struct FrameWorker<T, I> {
    rx: mpsc::Receiver<T>,
    info: I,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl<T: Send + 'static, I: Send + 'static> FrameWorker<T, I> {
    /// Spawns `body` on a thread named `name` and blocks (at most
    /// `ready_timeout`) until it calls [`WorkerContext::report_ready`].
    ///
    /// `body` returns when it is done or has failed. A failure before
    /// readiness comes back here as [`SourceError::Init`]; a failure after
    /// readiness is logged by this wrapper and surfaces to the consumer as
    /// the frame channel closing ([`Self::next`] returning `None`).
    pub fn spawn<F>(
        name: &str,
        capacity: usize,
        ready_timeout: Duration,
        body: F,
    ) -> Result<Self, SourceError>
    where
        F: FnOnce(&mut WorkerContext<T, I>) -> Result<(), String> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<I, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_name = name.to_string();
        let mut ctx = WorkerContext {
            frames: FrameSender::new(tx, name),
            stop: stop.clone(),
            ready: Some(ready_tx),
        };
        let join = std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                if let Err(e) = body(&mut ctx) {
                    match ctx.ready.take() {
                        Some(ready) => {
                            let _ = ready.send(Err(e));
                        }
                        None => eprintln!("[{thread_name}] worker stopped: {e}"),
                    }
                }
                // A body that returns Ok(()) without ever reporting ready
                // would otherwise leave the spawner waiting for the full
                // timeout; dropping `ctx` here closes the ready channel.
            })
            .map_err(|e| SourceError::Init(format!("spawn worker thread: {e}")))?;

        match ready_rx.recv_timeout(ready_timeout) {
            Ok(Ok(info)) => Ok(Self {
                rx,
                info,
                stop,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = join.join();
                Err(SourceError::Init(e))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                stop.store(true, Ordering::SeqCst);
                let _ = join.join();
                Err(SourceError::Init(
                    "worker exited without reporting readiness".into(),
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::SeqCst);
                // Don't join: the body may be stuck in an OS call; the
                // stop flag is its signal and the thread is detached.
                Err(SourceError::Init(format!(
                    "worker did not become ready within {ready_timeout:?}"
                )))
            }
        }
    }

    pub fn info(&self) -> &I {
        &self.info
    }

    /// Next frame, or `None` once the worker has stopped.
    pub async fn next(&mut self) -> Option<T> {
        self.rx.recv().await
    }

    /// The stop flag, for a caller that wants to signal without dropping.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
}

impl<T, I> Drop for FrameWorker<T, I> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn frames_flow_in_order_and_the_worker_stops_on_drop() {
        let worker = FrameWorker::<u64, &'static str>::spawn(
            "test-source",
            4,
            Duration::from_secs(5),
            |ctx| {
                assert!(ctx.report_ready("ready"));
                assert!(!ctx.report_ready("again"));
                let mut n = 0u64;
                while !ctx.should_stop() {
                    if ctx.frames.send(n) == SendOutcome::Closed {
                        break;
                    }
                    n += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            },
        )
        .expect("spawn");
        assert_eq!(*worker.info(), "ready");
        let mut worker = worker;
        let first = worker.next().await.expect("a frame");
        let second = worker.next().await.expect("a frame");
        assert!(second > first, "frames arrive in production order");
        let stop = worker.stop_flag();
        drop(worker);
        assert!(
            stop.load(Ordering::SeqCst),
            "drop requests a stop and joins"
        );
    }

    #[tokio::test]
    async fn a_slow_consumer_makes_the_source_drop_not_block() {
        // Producer fills the channel far faster than the consumer reads;
        // it must keep running (DR-007: drop at the source) and count.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<u64>();
        let mut worker =
            FrameWorker::<u64, ()>::spawn("test-burst", 2, Duration::from_secs(5), move |ctx| {
                ctx.report_ready(());
                let start = Instant::now();
                for n in 0..1000u64 {
                    if ctx.frames.send(n) == SendOutcome::Closed {
                        break;
                    }
                }
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "send never blocks"
                );
                let _ = done_tx.send(ctx.frames.dropped());
                Ok(())
            })
            .expect("spawn");
        let dropped = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("producer finished");
        assert!(
            dropped >= 998 - 2,
            "almost everything was dropped, got {dropped}"
        );
        // The consumer still gets what was queued, in order, then the end.
        let a = worker.next().await.expect("queued frame");
        let b = worker.next().await.expect("queued frame");
        assert!(b > a);
        assert_eq!(
            worker.next().await,
            None,
            "channel closes when the body returns"
        );
    }

    #[test]
    fn init_failure_is_reported_to_the_spawner() {
        let result =
            FrameWorker::<u64, ()>::spawn("test-fail", 4, Duration::from_secs(5), |_ctx| {
                Err("no capture device".to_string())
            });
        assert_eq!(
            result.err(),
            Some(SourceError::Init("no capture device".into()))
        );
    }

    #[test]
    fn a_body_that_exits_before_ready_is_an_init_failure() {
        let result =
            FrameWorker::<u64, ()>::spawn("test-early", 4, Duration::from_secs(5), |_ctx| Ok(()));
        assert!(matches!(result.err(), Some(SourceError::Init(_))));
    }

    #[test]
    fn readiness_timeout_is_an_init_failure_and_signals_stop() {
        let (seen_stop_tx, seen_stop_rx) = std::sync::mpsc::channel::<bool>();
        let result = FrameWorker::<u64, ()>::spawn(
            "test-timeout",
            4,
            Duration::from_millis(50),
            move |ctx| {
                // Never reports ready; leaves once told to stop.
                let start = Instant::now();
                while !ctx.should_stop() && start.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = seen_stop_tx.send(ctx.should_stop());
                Ok(())
            },
        );
        assert!(matches!(result.err(), Some(SourceError::Init(ref m)) if m.contains("ready")));
        assert!(seen_stop_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[tokio::test]
    async fn a_failure_after_ready_closes_the_channel() {
        let mut worker =
            FrameWorker::<u64, ()>::spawn("test-late-fail", 4, Duration::from_secs(5), |ctx| {
                ctx.report_ready(());
                ctx.frames.send(1);
                Err("display lost".to_string())
            })
            .expect("spawn succeeds: the failure comes after readiness");
        assert_eq!(worker.next().await, Some(1));
        assert_eq!(worker.next().await, None);
    }
}
