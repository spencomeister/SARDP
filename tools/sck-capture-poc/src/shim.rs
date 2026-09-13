//! Safe wrapper over the C ABI exported by `shim/SckShim.swift`.
//!
//! The whole ScreenCaptureKit session is one opaque handle; [`Session`]
//! owns it and stops/releases it in `Drop`, so an early return or a panic
//! on the Rust side still tears the OS capture down (3W-1 lesson 1).

use std::ffi::{CStr, c_char, c_void};
use std::fmt;

/// `SCFrameStatus` as delivered in the sample buffer attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameStatus {
    Complete,
    Idle,
    Blank,
    Suspended,
    Started,
    Stopped,
    Unknown(i32),
}

impl From<i32> for FrameStatus {
    fn from(v: i32) -> Self {
        match v {
            0 => Self::Complete,
            1 => Self::Idle,
            2 => Self::Blank,
            3 => Self::Suspended,
            4 => Self::Started,
            5 => Self::Stopped,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirtyRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// One delivered frame, valid only for the duration of the callback.
pub struct FrameRef<'a> {
    /// `None` for status frames that carry no image.
    pub bgra: Option<&'a [u8]>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub status: FrameStatus,
    pub display_time_ns: u64,
    pub dirty: &'a [DirtyRect],
    pub content_scale: f64,
    pub scale_factor: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra = 0,
    Nv12 = 1,
}

#[derive(Debug, Clone, Copy)]
pub struct MainDisplayInfo {
    pub display_id: u32,
    pub points_w: f64,
    pub points_h: f64,
    pub pixels_w: u32,
    pub pixels_h: u32,
    pub origin_x: f64,
    pub origin_y: f64,
    pub refresh_hz: f64,
}

#[derive(Debug)]
pub enum StartError {
    /// Screen Recording TCC permission is not granted for this process.
    NoPermission(String),
    NoDisplay(String),
    Failed(String),
    Timeout(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPermission(m) => write!(f, "no screen recording permission: {m}"),
            Self::NoDisplay(m) => write!(f, "no display: {m}"),
            Self::Failed(m) => write!(f, "start failed: {m}"),
            Self::Timeout(m) => write!(f, "start timed out: {m}"),
        }
    }
}

impl std::error::Error for StartError {}

type RawFrameCb = unsafe extern "C" fn(
    ctx: *mut c_void,
    bgra: *const u8,
    width: u32,
    height: u32,
    stride: u32,
    status: i32,
    display_time_ns: u64,
    dirty: *const f64,
    dirty_count: u32,
    content_scale: f64,
    scale_factor: f64,
);
type RawStoppedCb = unsafe extern "C" fn(ctx: *mut c_void, message: *const c_char);

unsafe extern "C" {
    fn sardp_sck_preflight() -> bool;
    fn sardp_sck_request_access() -> bool;
    fn sardp_sck_main_display_info(out: *mut f64) -> bool;
    fn sardp_sck_start(
        fps: u32,
        pixel_format: u32,
        shows_cursor: bool,
        timeout_ms: u32,
        skip_preflight: bool,
        ctx: *mut c_void,
        on_frame: RawFrameCb,
        on_stopped: Option<RawStoppedCb>,
        out_handle: *mut *mut c_void,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    fn sardp_sck_stop(handle: *mut c_void);
}

/// Read-only check; never shows a dialog.
pub fn preflight_screen_capture_access() -> bool {
    unsafe { sardp_sck_preflight() }
}

/// Triggers the system Screen Recording prompt if the decision is still
/// pending for this process's identity; returns the current state.
pub fn request_screen_capture_access() -> bool {
    unsafe { sardp_sck_request_access() }
}

pub fn main_display_info() -> Option<MainDisplayInfo> {
    let mut out = [0f64; 8];
    if !unsafe { sardp_sck_main_display_info(out.as_mut_ptr()) } {
        return None;
    }
    Some(MainDisplayInfo {
        display_id: out[0] as u32,
        points_w: out[1],
        points_h: out[2],
        pixels_w: out[3] as u32,
        pixels_h: out[4] as u32,
        origin_x: out[5],
        origin_y: out[6],
        refresh_hz: out[7],
    })
}

/// What the session calls back into. Both run on SCK's sample-handler
/// queue (a background thread), never on the caller's thread.
pub trait FrameSink: Send + 'static {
    fn on_frame(&mut self, frame: FrameRef<'_>);
    fn on_stopped(&mut self, message: &str);
}

/// A running ScreenCaptureKit capture of the main display.
pub struct Session {
    handle: *mut c_void,
    // Boxed sink, owned here so its address is stable for the callbacks;
    // freed only after `sardp_sck_stop` returned (no more callbacks).
    sink: *mut c_void,
    drop_sink: unsafe fn(*mut c_void),
}

// The handle is only ever touched from `Drop`; the sink is only touched
// from the callbacks (SCK's queue) while running and from `Drop` after.
unsafe impl Send for Session {}

impl Session {
    pub fn start<S: FrameSink>(
        fps: u32,
        pixel_format: PixelFormat,
        shows_cursor: bool,
        timeout_ms: u32,
        skip_preflight: bool,
        sink: S,
    ) -> Result<Self, StartError> {
        unsafe extern "C" fn frame_tramp<S: FrameSink>(
            ctx: *mut c_void,
            bgra: *const u8,
            width: u32,
            height: u32,
            stride: u32,
            status: i32,
            display_time_ns: u64,
            dirty: *const f64,
            dirty_count: u32,
            content_scale: f64,
            scale_factor: f64,
        ) {
            let sink = unsafe { &mut *(ctx as *mut S) };
            let bgra = if bgra.is_null() || height == 0 || stride == 0 {
                None
            } else {
                Some(unsafe { std::slice::from_raw_parts(bgra, stride as usize * height as usize) })
            };
            let dirty: Vec<DirtyRect> = if dirty.is_null() || dirty_count == 0 {
                Vec::new()
            } else {
                let raw = unsafe { std::slice::from_raw_parts(dirty, dirty_count as usize * 4) };
                raw.chunks_exact(4)
                    .map(|c| DirtyRect {
                        x: c[0],
                        y: c[1],
                        w: c[2],
                        h: c[3],
                    })
                    .collect()
            };
            sink.on_frame(FrameRef {
                bgra,
                width,
                height,
                stride,
                status: FrameStatus::from(status),
                display_time_ns,
                dirty: &dirty,
                content_scale,
                scale_factor,
            });
        }
        unsafe extern "C" fn stopped_tramp<S: FrameSink>(ctx: *mut c_void, message: *const c_char) {
            let sink = unsafe { &mut *(ctx as *mut S) };
            let msg = if message.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(message) }
                    .to_string_lossy()
                    .into_owned()
            };
            sink.on_stopped(&msg);
        }
        unsafe fn drop_sink<S: FrameSink>(p: *mut c_void) {
            drop(unsafe { Box::from_raw(p as *mut S) });
        }

        let sink = Box::into_raw(Box::new(sink)) as *mut c_void;
        let mut handle: *mut c_void = std::ptr::null_mut();
        let mut err = [0 as c_char; 512];
        let rc = unsafe {
            sardp_sck_start(
                fps,
                pixel_format as u32,
                shows_cursor,
                timeout_ms,
                skip_preflight,
                sink,
                frame_tramp::<S>,
                Some(stopped_tramp::<S>),
                &mut handle,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc != 0 {
            // No session was created, so no callback can be in flight.
            unsafe { drop_sink::<S>(sink) };
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(match rc {
                1 => StartError::NoPermission(msg),
                2 => StartError::NoDisplay(msg),
                4 => StartError::Timeout(msg),
                _ => StartError::Failed(msg),
            });
        }
        Ok(Self {
            handle,
            sink,
            drop_sink: drop_sink::<S>,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            sardp_sck_stop(self.handle);
            (self.drop_sink)(self.sink);
        }
    }
}
