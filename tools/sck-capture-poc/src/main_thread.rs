//! Marshals AppKit calls onto the process's real main thread (3M-1-e).
//!
//! An early standalone probe (a bare Swift executable, its `main()`
//! running on thread 0) suggested a window could be created and driven
//! from any background thread, as long as *something* periodically
//! serviced AppKit's event queue -- see `shim/DisplayWindow.swift`'s
//! original doc comment. Running the actual pipeline inside
//! `sardp-client` (whose real thread 0 is occupied by `#[tokio::main]`'s
//! runtime, so `sardp_mac::display`'s worker is a *spawned* thread, not
//! thread 0) surfaced the gap in that probe and crashed immediately:
//!
//! ```text
//! *** Terminating app due to uncaught exception 'NSInternalInconsistencyException',
//! reason: 'NSWindow should only be instantiated on the main thread!'
//! ```
//!
//! This is an Objective-C exception, which Rust cannot catch or unwind
//! through (`fatal runtime error: Rust cannot catch foreign exceptions,
//! aborting`) -- there is no `Result` to check first, so the earlier
//! probe's "it didn't error" was never going to be strong enough
//! evidence on its own (3W-1 lesson 2 again, just discovered a step
//! later than usual: the *first* check that mattered was "does creating
//! a window off thread 0 raise at all", not "does the window end up
//! visible").
//!
//! The fix is not a workaround for the assertion; it is doing what
//! AppKit actually requires: every AppKit call genuinely runs on thread
//! 0. `sardp-cli`'s `main`, on macOS with `--display window`, is
//! restructured so its real thread 0 runs [`run_main_thread_loop`] for
//! the life of the process, while the tokio runtime (and with it
//! `sardp_mac::display`'s worker thread) runs on a *spawned* thread
//! instead. [`crate::window::DisplayWindow`]'s methods each funnel their
//! FFI call through [`on_main_thread`], which blocks the calling thread
//! (never the main one) until the job has actually run there and
//! returned -- so from `sardp_mac::display`'s point of view, nothing
//! about calling `DisplayWindow` changes at all.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send>;

/// Set once, by [`run_main_thread_loop`], from the thread that calls it
/// (which must be the process's real main thread).
static JOB_TX: OnceLock<Sender<Job>> = OnceLock::new();

unsafe extern "C" {
    /// Drains AppKit's event queue without blocking. Not tied to any
    /// particular window (there may be none yet the first few times this
    /// runs) -- see `DisplayWindow.swift`.
    fn sardp_appkit_pump_main_thread();
}

/// Services AppKit's event queue and any job [`on_main_thread`] enqueues,
/// until `should_exit` is set. **Must be called on the process's real
/// main thread**, and at most once per process, before the first
/// [`crate::window::DisplayWindow`] is created from any other thread.
pub fn run_main_thread_loop(should_exit: &AtomicBool) {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = mpsc::channel();
    JOB_TX
        .set(tx)
        .unwrap_or_else(|_| panic!("run_main_thread_loop called more than once per process"));
    while !should_exit.load(Ordering::SeqCst) {
        unsafe { sardp_appkit_pump_main_thread() };
        while let Ok(job) = rx.try_recv() {
            job();
        }
        // Not a vsync-rate poll like a display's own frame loop -- this
        // just needs to notice new jobs and window-server events
        // promptly; 5ms keeps CPU use negligible without adding
        // perceptible latency to window creation or a frame present.
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Runs `f` on the process's real main thread and returns its result,
/// blocking the *calling* thread (never the main one) until it has.
///
/// # Panics
/// If [`run_main_thread_loop`] is not currently running (called before it
/// started, after it exited, or from the main thread loop itself, which
/// would deadlock waiting on its own queue).
pub fn on_main_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let tx = JOB_TX
        .get()
        .expect("on_main_thread called before run_main_thread_loop started");
    let (result_tx, result_rx) = mpsc::channel();
    tx.send(Box::new(move || {
        // The receiving end only goes away if `on_main_thread`'s own
        // caller already stopped waiting, which does not happen here.
        let _ = result_tx.send(f());
    }))
    .expect("run_main_thread_loop is not running (its receiver is gone)");
    result_rx
        .recv()
        .expect("run_main_thread_loop dropped the job without running it")
}
