//! Safe wrapper over the C ABI exported by `shim/DisplayWindow.swift`.
//!
//! Every method funnels its actual FFI call through
//! [`crate::main_thread::on_main_thread`] -- see that module's doc, and
//! `DisplayWindow.swift`'s, for why: `NSWindow` enforces (an uncaught
//! Objective-C exception, not a `Result`) that it is only ever touched
//! from the process's real main thread, which is not the thread
//! `sardp_mac::display`'s worker owns. From that worker's point of view
//! nothing about calling this type changes; every call just now blocks
//! briefly while the real main thread's loop picks the job up and runs
//! it there.
//!
//! Released in `Drop`, same discipline as the other shim wrappers, so an
//! early return or a panic still closes the window rather than leaking
//! it (3W-1 lesson 1) -- `Drop` itself still runs on whichever thread
//! drops the value, same as every other method; only the FFI call inside
//! it hops to the main thread.

use std::ffi::{CString, c_char, c_void};

use crate::main_thread::on_main_thread;
use crate::vtdec::PixelBufferOwned;

/// A raw pointer that is safe to move into an [`on_main_thread`] job:
/// it is only ever dereferenced by Swift, on the main thread, one job at
/// a time -- there is no concurrent access for Rust's usual `Send`
/// guarantees to be protecting against.
struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}

impl SendPtr {
    /// A method, not a bare `.0` field access: Rust's precise closure
    /// captures (RFC 2229) would otherwise capture just the `*mut c_void`
    /// field itself (not `Send`) rather than this whole newtype, quietly
    /// defeating the wrapper. A method call always captures the receiver
    /// as a whole.
    fn get(&self) -> *mut c_void {
        self.0
    }
}

/// A visible, on-screen window presenting decoded frames.
pub struct DisplayWindow {
    handle: SendPtr,
}

// See `SendPtr`: the handle itself is safe to hand across the
// main-thread boundary, and `sardp_mac::display` only ever touches a
// `DisplayWindow` from the one worker thread that created it.
unsafe impl Send for DisplayWindow {}

unsafe extern "C" {
    fn sardp_win_create(width_pt: i32, height_pt: i32, title: *const c_char) -> *mut c_void;
    fn sardp_win_present(handle: *mut c_void, pixel_buffer: *mut c_void) -> bool;
    fn sardp_win_pump(handle: *mut c_void);
    fn sardp_win_is_visible(handle: *mut c_void) -> bool;
    fn sardp_win_should_close(handle: *mut c_void) -> bool;
    fn sardp_win_destroy(handle: *mut c_void);
}

impl DisplayWindow {
    /// Creates and shows a window. `width_pt`/`height_pt` are the
    /// client-area size in points (frames are scaled to fit, independent
    /// of the stream's own resolution -- same contract as
    /// `sardp_win::DisplayConfig`).
    ///
    /// Blocks the calling thread briefly while the main thread's loop
    /// (which must already be running -- `sardp-cli`'s `main` on macOS
    /// starts it before spawning the thread that eventually calls this)
    /// picks the job up.
    pub fn create(width_pt: u32, height_pt: u32, title: &str) -> Self {
        let c_title = CString::new(title).unwrap_or_default();
        let width = width_pt as i32;
        let height = height_pt as i32;
        let handle = on_main_thread(move || {
            SendPtr(unsafe { sardp_win_create(width, height, c_title.as_ptr()) })
        });
        Self { handle }
    }

    /// Presents a decoded frame (consumes it -- see
    /// [`PixelBufferOwned`]'s doc). Returns whether it was actually
    /// IOSurface-backed and could be shown; `false` is worth logging.
    pub fn present(&self, frame: PixelBufferOwned) -> bool {
        let handle = SendPtr(self.handle.get());
        let buffer = SendPtr(frame.into_raw());
        on_main_thread(move || unsafe { sardp_win_present(handle.get(), buffer.get()) })
    }

    /// Drains pending window-server events without blocking. Call at
    /// least as often as the frame-wait timeout, same discipline
    /// `sardp-win`'s `pump_messages()` follows.
    pub fn pump(&self) {
        let handle = SendPtr(self.handle.get());
        on_main_thread(move || unsafe { sardp_win_pump(handle.get()) });
    }

    /// Ground truth from the window server, not an assumption from
    /// `create` having returned without error (3W-1 lesson 2: an API's
    /// own account of success is not to be trusted at face value --
    /// restated once more, one layer down, by this whole module's doc).
    pub fn is_visible(&self) -> bool {
        let handle = SendPtr(self.handle.get());
        on_main_thread(move || unsafe { sardp_win_is_visible(handle.get()) })
    }

    /// Whether the user clicked the close box.
    pub fn should_close(&self) -> bool {
        let handle = SendPtr(self.handle.get());
        on_main_thread(move || unsafe { sardp_win_should_close(handle.get()) })
    }
}

impl Drop for DisplayWindow {
    fn drop(&mut self) {
        let handle = SendPtr(self.handle.get());
        on_main_thread(move || unsafe { sardp_win_destroy(handle.get()) });
    }
}
