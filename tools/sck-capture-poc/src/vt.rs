//! Safe wrapper over the C ABI exported by `shim/VtEncoder.swift`.
//!
//! Same shape as [`crate::shim::Session`]: one opaque handle owned by
//! [`Encoder`], flushed/invalidated/released in `Drop` so an early return
//! or a panic still tears the VideoToolbox session down (3W-1 lesson 1).
//!
//! Encoded output is *pushed*: VideoToolbox calls back on its own thread
//! once a frame is done, rather than the caller pulling it. There is
//! therefore no "collect what is already ready on the next call" step and
//! none of the one-frame lag DR-036 found on the Media Foundation path --
//! a frame leaves as soon as the encoder is finished with it.

use std::ffi::{CStr, c_char, c_void};
use std::fmt;

use crate::shim::PixelBufferRef;

/// One encoded access unit, valid only for the duration of the callback.
pub struct EncodedRef<'a> {
    /// Annex-B bytes (the shim converted VideoToolbox's AVCC length
    /// prefixes to start codes).
    pub annex_b: &'a [u8],
    /// VideoToolbox's own keyframe claim (`NotSync` attachment absent).
    /// A hint: `sardp::h264::is_idr_access_unit` decides from the bytes.
    pub is_sync: bool,
    /// The value passed to [`Encoder::encode`] for this frame, carried
    /// through as the sample's presentation stamp.
    pub capture_ts_us: u64,
    /// SPS+PPS as Annex-B, present only on the first frame and whenever
    /// they change. VideoToolbox keeps them out of the bitstream, so this
    /// is the only place they come from.
    pub parameter_sets: Option<&'a [u8]>,
}

/// What the encoder calls back into, on a VideoToolbox thread (never the
/// caller's). Calls for one encoder are serialized.
pub trait EncodedSink: Send + 'static {
    fn on_encoded(&mut self, frame: EncodedRef<'_>);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncoderStats {
    pub inputs: u64,
    pub outputs: u64,
    /// Frames VideoToolbox threw away itself (`kVTEncodeInfo_FrameDropped`);
    /// distinct from the source-side drops of DR-007.
    pub dropped_by_encoder: u64,
    /// Output callbacks that arrived with a non-zero `OSStatus`.
    pub callback_errors: u64,
}

#[derive(Debug)]
pub enum EncodeError {
    Create(String),
    Property(String),
    /// `VTCompressionSessionEncodeFrame` refused the frame.
    Encode(i32),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create(m) => write!(f, "VideoToolbox session creation failed: {m}"),
            Self::Property(m) => write!(f, "VideoToolbox property rejected: {m}"),
            Self::Encode(rc) => write!(f, "VideoToolbox encode failed (rc {rc})"),
        }
    }
}

impl std::error::Error for EncodeError {}

type RawEncodedCb = unsafe extern "C" fn(
    ctx: *mut c_void,
    annex_b: *const u8,
    len: usize,
    is_sync: bool,
    capture_ts_us: u64,
    param_sets: *const u8,
    param_sets_len: usize,
);

unsafe extern "C" {
    fn sardp_vt_encoder_create(
        width: i32,
        height: i32,
        fps: u32,
        bitrate_bps: i32,
        all_idr: bool,
        ctx: *mut c_void,
        on_encoded: RawEncodedCb,
        out_handle: *mut *mut c_void,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    fn sardp_vt_encoder_encode(
        handle: *mut c_void,
        pixel_buffer: *mut c_void,
        capture_ts_us: u64,
        force_idr: bool,
    ) -> i32;
    fn sardp_vt_encoder_uses_hardware(handle: *mut c_void) -> bool;
    fn sardp_vt_encoder_stats(handle: *mut c_void, out: *mut u64);
    fn sardp_vt_encoder_complete(handle: *mut c_void);
    fn sardp_vt_encoder_destroy(handle: *mut c_void);
}

/// A running VideoToolbox H.264 compression session.
pub struct Encoder {
    handle: *mut c_void,
    // Boxed sink, owned here so its address is stable for the callback;
    // freed only after `sardp_vt_encoder_destroy` returned, which flushes
    // and invalidates the session and so guarantees no callback is still
    // in flight.
    sink: *mut c_void,
    drop_sink: unsafe fn(*mut c_void),
}

// The handle is used from whichever thread drives capture; the sink is
// only ever touched from the VideoToolbox callback (serialized by the
// shim) and from `Drop` after the session is gone.
unsafe impl Send for Encoder {}
// `VTCompressionSession` is thread-safe, and the shim guards its own
// counters, so `&Encoder` may be shared: typically the capture callback
// calls `encode` while the owning thread reads `stats`.
unsafe impl Sync for Encoder {}

impl Encoder {
    pub fn new<S: EncodedSink>(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        all_idr: bool,
        sink: S,
    ) -> Result<Self, EncodeError> {
        unsafe extern "C" fn encoded_tramp<S: EncodedSink>(
            ctx: *mut c_void,
            annex_b: *const u8,
            len: usize,
            is_sync: bool,
            capture_ts_us: u64,
            param_sets: *const u8,
            param_sets_len: usize,
        ) {
            let sink = unsafe { &mut *(ctx as *mut S) };
            if annex_b.is_null() || len == 0 {
                return;
            }
            let bytes = unsafe { std::slice::from_raw_parts(annex_b, len) };
            let sets = if param_sets.is_null() || param_sets_len == 0 {
                None
            } else {
                Some(unsafe { std::slice::from_raw_parts(param_sets, param_sets_len) })
            };
            sink.on_encoded(EncodedRef {
                annex_b: bytes,
                is_sync,
                capture_ts_us,
                parameter_sets: sets,
            });
        }
        unsafe fn drop_sink<S: EncodedSink>(p: *mut c_void) {
            drop(unsafe { Box::from_raw(p as *mut S) });
        }

        let sink = Box::into_raw(Box::new(sink)) as *mut c_void;
        let mut handle: *mut c_void = std::ptr::null_mut();
        let mut err = [0 as c_char; 512];
        let rc = unsafe {
            sardp_vt_encoder_create(
                width as i32,
                height as i32,
                fps,
                bitrate_bps.min(i32::MAX as u32) as i32,
                all_idr,
                sink,
                encoded_tramp::<S>,
                &mut handle,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc != 0 {
            // No session exists, so no callback can be in flight.
            unsafe { drop_sink::<S>(sink) };
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(match rc {
                2 => EncodeError::Property(msg),
                _ => EncodeError::Create(msg),
            });
        }
        Ok(Self {
            handle,
            sink,
            drop_sink: drop_sink::<S>,
        })
    }

    /// Submits one frame. The pixel buffer comes from a
    /// [`crate::shim::FrameRef`] and is only valid inside that frame
    /// callback, which its lifetime enforces -- so this is meant to be
    /// called from there. Returning `Ok` means the encoder accepted the
    /// frame, not that one comes out: it may still drop it internally
    /// (see [`Self::stats`]).
    pub fn encode(
        &self,
        pixel_buffer: PixelBufferRef<'_>,
        capture_ts_us: u64,
        force_idr: bool,
    ) -> Result<(), EncodeError> {
        let rc = unsafe {
            sardp_vt_encoder_encode(self.handle, pixel_buffer.as_ptr(), capture_ts_us, force_idr)
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(EncodeError::Encode(rc))
        }
    }

    /// Whether the session actually landed on the hardware encoder. Read
    /// back from VideoToolbox, not the flag we asked for.
    pub fn uses_hardware(&self) -> bool {
        unsafe { sardp_vt_encoder_uses_hardware(self.handle) }
    }

    pub fn stats(&self) -> EncoderStats {
        let mut out = [0u64; 4];
        unsafe { sardp_vt_encoder_stats(self.handle, out.as_mut_ptr()) };
        EncoderStats {
            inputs: out[0],
            outputs: out[1],
            dropped_by_encoder: out[2],
            callback_errors: out[3],
        }
    }

    /// Flushes frames still inside the encoder; their callbacks run before
    /// this returns.
    pub fn complete(&self) {
        unsafe { sardp_vt_encoder_complete(self.handle) };
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            // Flushes, invalidates (the documented teardown) and releases.
            sardp_vt_encoder_destroy(self.handle);
            (self.drop_sink)(self.sink);
        }
    }
}
