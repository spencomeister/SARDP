//! Safe wrapper over the C ABI exported by `shim/CgInject.swift`:
//! the Accessibility TCC gate, CGEvent posting, and the listen-only event
//! tap the PoC verifies with.
//!
//! As with [`crate::shim::Session`] and [`crate::vt::Encoder`], the OS
//! handle is owned by a value that releases it in `Drop`. What is *not*
//! here is any policy: which modifier flags a key sets, whether a move is
//! a drag, what the click count is. That lives in `sardp-mac::inject`,
//! where it is ordinary Rust with unit tests.

use std::ffi::{CStr, c_char, c_void};
use std::fmt;

/// The `kCGEventSourceUserData` tag on everything posted through here
/// ("SARD"), the macOS counterpart of `dwExtraInfo` on Windows. A client
/// on the same machine reads it back to break the echo loop.
pub const INJECTED_USER_DATA: i64 = 0x5341_5244;

/// Which mouse event to post. The caller decides between a move and a
/// drag, because that depends on the buttons it is tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Move = 0,
    Down = 1,
    Up = 2,
    Drag = 3,
}

#[derive(Debug)]
pub enum InjectError {
    /// Accessibility is not granted for this process's code identity.
    NotTrusted(String),
    Failed(String),
    /// A post was refused; the OS gave no detail beyond the code.
    Post(i32),
}

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTrusted(m) => write!(f, "accessibility permission not granted: {m}"),
            Self::Failed(m) => write!(f, "input injection setup failed: {m}"),
            Self::Post(rc) => write!(f, "CGEvent post failed (rc {rc})"),
        }
    }
}

impl std::error::Error for InjectError {}

type RawTapCb = unsafe extern "C" fn(
    ctx: *mut c_void,
    event_type: u32,
    key_code: i64,
    flags: u64,
    x: f64,
    y: f64,
    click_state: i64,
    user_data: i64,
);

unsafe extern "C" {
    fn sardp_ax_is_trusted() -> bool;
    fn sardp_ax_request_trust() -> bool;
    fn sardp_cg_injector_create(
        out_handle: *mut *mut c_void,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    fn sardp_cg_injector_destroy(handle: *mut c_void);
    fn sardp_cg_post_key(handle: *mut c_void, key_code: u16, down: bool, flags: u64) -> i32;
    fn sardp_cg_post_text(handle: *mut c_void, utf8: *const u8, len: usize) -> i32;
    fn sardp_cg_post_mouse(
        handle: *mut c_void,
        kind: u32,
        button: u32,
        x: f64,
        y: f64,
        click_state: i64,
        flags: u64,
    ) -> i32;
    fn sardp_cg_post_scroll(handle: *mut c_void, lines_dy: i32, lines_dx: i32, flags: u64) -> i32;
    fn sardp_cg_cursor_position(out: *mut f64) -> bool;
    fn sardp_cg_tap_start(
        ctx: *mut c_void,
        callback: RawTapCb,
        consume_tagged: bool,
        out_handle: *mut *mut c_void,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    fn sardp_cg_tap_stop(handle: *mut c_void);
}

/// Whether this process may post events. Never shows a dialog.
pub fn is_accessibility_trusted() -> bool {
    unsafe { sardp_ax_is_trusted() }
}

/// Asks for Accessibility, showing the system prompt if the decision is
/// still pending. Returns the state *now*: for a first request that is
/// `false`, because the grant is made afterwards in System Settings.
///
/// Unlike Screen Recording (KNOWN_ISSUES #20), this prompt really is
/// shown -- `universalAccessAuthWarn` puts up the dialog.
pub fn request_accessibility_trust() -> bool {
    unsafe { sardp_ax_request_trust() }
}

/// Current cursor position in global display points.
pub fn cursor_position() -> Option<(f64, f64)> {
    let mut out = [0f64; 2];
    unsafe { sardp_cg_cursor_position(out.as_mut_ptr()) }.then_some((out[0], out[1]))
}

/// A CGEvent source, and the right to post through it.
pub struct Injector {
    handle: *mut c_void,
}

// The handle is a `CGEventSource`, which is safe to use from any thread;
// the injector is owned by one thread in practice (`sardp-mac` runs it on
// a dedicated one so text pacing cannot block the async loop).
unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Result<Self, InjectError> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let mut err = [0 as c_char; 256];
        let rc = unsafe { sardp_cg_injector_create(&mut handle, err.as_mut_ptr(), err.len()) };
        if rc != 0 {
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(match rc {
                1 => InjectError::NotTrusted(msg),
                _ => InjectError::Failed(msg),
            });
        }
        Ok(Self { handle })
    }

    /// One key down/up by macOS virtual key code, with `flags` as the
    /// complete modifier state the caller tracks.
    pub fn key(&self, key_code: u16, down: bool, flags: u64) -> Result<(), InjectError> {
        check(unsafe { sardp_cg_post_key(self.handle, key_code, down, flags) })
    }

    /// Committed text, as one keyboard event carrying the characters.
    pub fn text(&self, text: &str) -> Result<(), InjectError> {
        check(unsafe { sardp_cg_post_text(self.handle, text.as_ptr(), text.len()) })
    }

    /// One mouse event at global point coordinates.
    pub fn mouse(
        &self,
        kind: MouseKind,
        button: u32,
        x: f64,
        y: f64,
        click_state: i64,
        flags: u64,
    ) -> Result<(), InjectError> {
        check(unsafe {
            sardp_cg_post_mouse(self.handle, kind as u32, button, x, y, click_state, flags)
        })
    }

    /// Scroll in line units, positive = up / right.
    pub fn scroll(&self, lines_dy: i32, lines_dx: i32, flags: u64) -> Result<(), InjectError> {
        check(unsafe { sardp_cg_post_scroll(self.handle, lines_dy, lines_dx, flags) })
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        unsafe { sardp_cg_injector_destroy(self.handle) };
    }
}

fn check(rc: i32) -> Result<(), InjectError> {
    if rc == 0 {
        Ok(())
    } else {
        Err(InjectError::Post(rc))
    }
}

/// One event seen by an [`EventTap`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TappedEvent {
    /// `CGEventType` raw value (10 = keyDown, 11 = keyUp, 12 = flagsChanged,
    /// 1 = leftMouseDown, 5 = mouseMoved, 22 = scrollWheel, ...).
    pub event_type: u32,
    /// Virtual key code for keyboard events, button number for mouse ones.
    pub key_code: i64,
    pub flags: u64,
    pub x: f64,
    pub y: f64,
    /// Click count on a mouse event (0 otherwise).
    pub click_state: i64,
    /// `kCGEventSourceUserData`; [`INJECTED_USER_DATA`] for events this
    /// process posted.
    pub user_data: i64,
}

pub trait TapSink: Send + 'static {
    fn on_event(&mut self, event: TappedEvent);
}

/// A session event tap. Exists to *verify* injection: posting an event
/// tells you nothing about whether it reached the system, so the PoC reads
/// its own events back out of the session tap.
///
/// In [`EventTap::start_filtering`] mode it also discards the events it
/// recognises as ours, which is what makes an end-to-end input test safe
/// on a machine someone is using -- the synthetic clicks and keystrokes
/// never reach an application. Real input always passes through.
pub struct EventTap {
    handle: *mut c_void,
    sink: *mut c_void,
    drop_sink: unsafe fn(*mut c_void),
}

unsafe impl Send for EventTap {}

impl EventTap {
    /// Observes events and lets every one through.
    pub fn start<S: TapSink>(sink: S) -> Result<Self, InjectError> {
        Self::create(sink, false)
    }

    /// Observes events and discards the ones tagged
    /// [`INJECTED_USER_DATA`], so this process's own injected input never
    /// reaches an application.
    pub fn start_filtering<S: TapSink>(sink: S) -> Result<Self, InjectError> {
        Self::create(sink, true)
    }

    fn create<S: TapSink>(sink: S, consume_tagged: bool) -> Result<Self, InjectError> {
        unsafe extern "C" fn tramp<S: TapSink>(
            ctx: *mut c_void,
            event_type: u32,
            key_code: i64,
            flags: u64,
            x: f64,
            y: f64,
            click_state: i64,
            user_data: i64,
        ) {
            let sink = unsafe { &mut *(ctx as *mut S) };
            sink.on_event(TappedEvent {
                event_type,
                key_code,
                flags,
                x,
                y,
                click_state,
                user_data,
            });
        }
        unsafe fn drop_sink<S: TapSink>(p: *mut c_void) {
            drop(unsafe { Box::from_raw(p as *mut S) });
        }

        let sink = Box::into_raw(Box::new(sink)) as *mut c_void;
        let mut handle: *mut c_void = std::ptr::null_mut();
        let mut err = [0 as c_char; 256];
        let rc = unsafe {
            sardp_cg_tap_start(
                sink,
                tramp::<S>,
                consume_tagged,
                &mut handle,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc != 0 {
            unsafe { drop_sink::<S>(sink) };
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(match rc {
                1 => InjectError::NotTrusted(msg),
                _ => InjectError::Failed(msg),
            });
        }
        Ok(Self {
            handle,
            sink,
            drop_sink: drop_sink::<S>,
        })
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        unsafe {
            // Stops the run loop and invalidates the mach port, so no
            // callback can be running once this returns.
            sardp_cg_tap_stop(self.handle);
            (self.drop_sink)(self.sink);
        }
    }
}
