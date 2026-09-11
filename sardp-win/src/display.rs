//! H.264 decode + on-screen display as a *sink*: the client-side mirror
//! of [`crate::desktop_h264`].
//!
//! [`H264DisplayWindow::open`] spawns a dedicated OS thread that owns a
//! Win32 window, a D3D11 device + flip-model swap chain, and a Media
//! Foundation H.264 decoder MFT given the D3D11 device manager (so decoded
//! frames land in GPU NV12 textures). Each decoded frame is blitted
//! NV12->BGRA straight into the swap chain's back buffer with
//! `ID3D11VideoProcessor` and presented -- no CPU-side pixels anywhere.
//! The thread reports per-frame decode/present timestamps back so the
//! client can build spec 2.14 `TransportFeedback` from real numbers.
//!
//! The decoder is persistent for the window's lifetime (DR-036: no
//! per-frame process spawn), which is also what makes P-frames decodable
//! at all -- the earlier per-frame `ffmpeg` decoder could only ever handle
//! self-contained IDRs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, IDXGISwapChain2, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager,
    IMFMediaType, IMFSample, IMFTransform, MF_LOW_LATENCY,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFShutdown,
    MFStartup, MFTEnumEx, MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_VERSION, MFSTARTUP_FULL,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::Ime::{
    GCS_COMPSTR, GCS_CURSORPOS, HIMC, ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageExtraInfo, GetWindowLongPtrW, PeekMessageW, PostQuitMessage, RegisterClassW,
    SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    GWLP_USERDATA, MSG, PM_REMOVE, SW_SHOW, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_IME_COMPOSITION,
    WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_CAPTION, WS_EX_LEFT, WS_MINIMIZEBOX,
    WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
};

use dxgi_capture_poc::capture::create_d3d11_device;

use crate::desktop_h264::Clock;
use crate::inject::{button, modifier, INJECTED_EXTRA_INFO};
use crate::keymap;

/// Input the user gave the window (3W-1-d-4), in window client-area
/// pixels; the client maps positions to the stream's pixel space and
/// turns these into spec 2.12 messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowInput {
    /// A physical key. `hid_usage` is the USB HID usage (spec 2.12
    /// `scancode`), `virtual_key` the Windows VK code (platform-local
    /// `logical_key`), `modifiers` per `inject::modifier`.
    Key {
        down: bool,
        hid_usage: u32,
        virtual_key: u32,
        modifiers: u16,
    },
    /// Committed text (`WM_CHAR`, including IME results), control
    /// characters excluded -- those keys travel as `Key`.
    Text(String),
    /// In-progress IME composition (`WM_IME_COMPOSITION`); empty text
    /// when the composition ends.
    ImeComposition { text: String, caret: u16 },
    MouseMove { x: i32, y: i32 },
    /// `button` per `inject::button`.
    MouseButton { button: u8, down: bool, x: i32, y: i32 },
    /// `WHEEL_DELTA` (120) units.
    Wheel { dx: i16, dy: i16 },
    /// The window lost keyboard focus: whatever the client reported as
    /// held down should be released on the remote side.
    FocusLost,
}

/// Per-window state the window procedure needs, reachable through
/// `GWLP_USERDATA`.
struct WindowState {
    input_tx: mpsc::UnboundedSender<WindowInput>,
    /// DR-025: while an IME composition is in progress, the physical keys
    /// feeding it are not reported as `Key` events.
    composing: bool,
    /// `WM_CHAR` delivers a non-BMP character as two messages.
    pending_high_surrogate: Option<u16>,
    buttons_down: u8,
}

/// Detaches and frees the `WindowState` when the display thread exits.
struct UserDataGuard {
    hwnd: HWND,
    state: *mut WindowState,
}

impl Drop for UserDataGuard {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(self.state));
        }
    }
}

#[derive(Debug, Clone)]
pub struct DisplayConfig {
    pub title: String,
    /// Client-area size of the window. Frames are scaled to fit by the
    /// video processor, so this is independent of the stream's size.
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
/// identity the timing report echoes back.
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

/// Per-frame timestamps from the display thread (client clock, same basis
/// as `receive_ts`), for `TransportFeedback` (spec 2.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    pub generation: u64,
    pub frame_id: u64,
    pub receive_ts: u64,
    /// When the display thread picked the frame up (so `dequeue_ts -
    /// receive_ts` is pure queueing and `decode_done_ts - dequeue_ts` the
    /// decoder's own time).
    pub dequeue_ts: u64,
    pub decode_done_ts: u64,
    pub display_ts: u64,
    /// False when the frame was decoded (keeping the reference chain
    /// intact) but not shown, because the swap chain still held the
    /// previous frame -- the decoder ran ahead of the display refresh.
    pub presented: bool,
}

#[derive(Debug)]
pub enum WinDisplayError {
    Init(String),
    /// The window was closed (by the user) or its thread died.
    Closed,
}

impl std::fmt::Display for WinDisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(s) => write!(f, "display init failed: {s}"),
            Self::Closed => write!(f, "display window closed"),
        }
    }
}

/// Per-step tracing of the display thread (`SARDP_WIN_TRACE=1`), for
/// diagnosing where a frame stalls; off by default since it's per frame.
fn trace(msg: impl std::fmt::Display) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("SARDP_WIN_TRACE").is_some()) {
        eprintln!("[sardp-win trace] {msg}");
    }
}

/// Shared between the submitting side and the display thread.
struct Shared {
    closed: AtomicBool,
    /// Highest generation submitted so far. Queued frames of an older
    /// generation are skipped by the display thread: once the server has
    /// reset and reopened (spec 2.10), decoding the leftovers of the old
    /// generation would only add latency to the new one.
    newest_generation: AtomicU64,
}

pub struct H264DisplayWindow {
    /// Unbounded on purpose: a frame is never dropped here. In a P-frame
    /// stream a dropped frame would corrupt everything up to the next IDR,
    /// and the client has no way to ask for one; instead a backlog shows
    /// up as `client_queue_delay_us` in `TransportFeedback`, and the
    /// server's backpressure (spec 2.10) resets the stream and opens a
    /// new generation, which is what makes the backlog skippable.
    tx: std::sync::mpsc::Sender<SubmittedFrame>,
    timing_rx: mpsc::Receiver<FrameTiming>,
    /// Taken by the client with [`Self::take_input_receiver`] so it can
    /// be polled independently of [`Self::next_timing`].
    input_rx: Option<mpsc::UnboundedReceiver<WindowInput>>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl H264DisplayWindow {
    /// Creates the window (and everything behind it) on its own thread and
    /// returns once it is on screen and a decoder MFT has been located.
    pub fn open(config: DisplayConfig, clock: Clock) -> Result<Self, WinDisplayError> {
        let (tx, rx) = std::sync::mpsc::channel::<SubmittedFrame>();
        let (timing_tx, timing_rx) = mpsc::channel::<FrameTiming>(64);
        // Unbounded: key events must never be dropped (a lost key-up is a
        // stuck key), and the volume is tiny next to video.
        let (input_tx, input_rx) = mpsc::unbounded_channel::<WindowInput>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let shared = Arc::new(Shared {
            closed: AtomicBool::new(false),
            newest_generation: AtomicU64::new(0),
        });

        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("sardp-win-display".into())
                .spawn(move || worker_main(config, clock, rx, timing_tx, input_tx, ready_tx, shared))
                .map_err(|e| WinDisplayError::Init(format!("spawn display thread: {e}")))?
        };

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(WinDisplayError::Init(e));
            }
            Err(_) => {
                return Err(WinDisplayError::Init(
                    "display thread did not become ready within 15s".into(),
                ))
            }
        }

        Ok(Self {
            tx,
            timing_rx,
            input_rx: Some(input_rx),
            shared,
            worker: Some(worker),
        })
    }

    /// The window's input events, as a receiver the caller owns (so it
    /// can sit in a `select!` next to [`Self::next_timing`]). `None` after
    /// the first call. Ends (`recv() == None`) when the window is gone.
    pub fn take_input_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<WindowInput>> {
        self.input_rx.take()
    }

    /// Queues a frame for decode+display. Never blocks and never drops
    /// (see the `tx` field); frames of a generation older than the newest
    /// submitted one are skipped by the display thread and get no timing.
    pub fn submit(&self, frame: SubmittedFrame) -> Result<(), WinDisplayError> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(WinDisplayError::Closed);
        }
        self.shared
            .newest_generation
            .fetch_max(frame.generation, Ordering::SeqCst);
        self.tx.send(frame).map_err(|_| WinDisplayError::Closed)
    }

    /// Next decode/present timing report; `None` once the window has been
    /// closed (by the user) and its thread has exited.
    pub async fn next_timing(&mut self) -> Option<FrameTiming> {
        self.timing_rx.recv().await
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

impl Drop for H264DisplayWindow {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        // Dropping `tx` here ends the worker's receive loop; join it.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_main(
    config: DisplayConfig,
    clock: Clock,
    rx: std::sync::mpsc::Receiver<SubmittedFrame>,
    timing_tx: mpsc::Sender<FrameTiming>,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    shared: Arc<Shared>,
) {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        let _ = ready_tx.send(Err(format!("MFStartup: {e}")));
        if com_ok {
            unsafe { CoUninitialize() };
        }
        return;
    }

    let mut ready_tx = Some(ready_tx);
    if let Err(e) = run_worker(config, clock, rx, timing_tx, input_tx, &mut ready_tx, &shared) {
        if let Some(ready) = ready_tx.take() {
            let _ = ready.send(Err(e));
        } else {
            eprintln!("[sardp-win] display thread stopped: {e}");
        }
    }
    shared.closed.store(true, Ordering::SeqCst);

    unsafe {
        let _ = MFShutdown();
        if com_ok {
            CoUninitialize();
        }
    }
}

/// Everything the display thread holds between frames. `device`/`context`
/// are kept alive here (the swap chain, decoder and video processor all
/// hang off them) even though nothing calls them directly after setup.
struct Presenter {
    #[allow(dead_code)]
    device: ID3D11Device,
    #[allow(dead_code)]
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    device_manager: IMFDXGIDeviceManager,
    swap_chain: IDXGISwapChain1,
    /// Signaled while the swap chain can take another Present without
    /// blocking (`DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`,
    /// maximum frame latency 1). Polled with a zero timeout so the decode
    /// thread never waits on vsync: with a blocking Present the thread was
    /// capped at one frame per refresh, equal to the capture rate, so a
    /// backlog could never drain.
    frame_latency_waitable: HANDLE,
    window_width: u32,
    window_height: u32,
    decoder: Option<Decoder>,
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // Documented requirement for GetFrameLatencyWaitableObject.
        let _ = unsafe { CloseHandle(self.frame_latency_waitable) };
    }
}

struct Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    frames_decoded: u64,
}

fn run_worker(
    config: DisplayConfig,
    clock: Clock,
    rx: std::sync::mpsc::Receiver<SubmittedFrame>,
    timing_tx: mpsc::Sender<FrameTiming>,
    input_tx: mpsc::UnboundedSender<WindowInput>,
    ready_tx: &mut Option<std::sync::mpsc::Sender<Result<(), String>>>,
    shared: &Shared,
) -> Result<(), String> {
    let e = |ctx: &str, err: windows::core::Error| format!("{ctx}: {err}");

    let hwnd = create_window(&config).map_err(|err| e("create window", err))?;
    trace(format!("window created: hwnd={:?}", hwnd.0));
    let state = Box::into_raw(Box::new(WindowState {
        input_tx,
        composing: false,
        pending_high_surrogate: None,
        buttons_down: 0,
    }));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
    let _user_data = UserDataGuard { hwnd, state };

    let (device, context) = create_d3d11_device(
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    )
    .map_err(|err| e("D3D11CreateDevice", err))?;
    let multithread: ID3D11Multithread = device.cast().map_err(|err| e("ID3D11Multithread", err))?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    let video_device: ID3D11VideoDevice = device.cast().map_err(|err| e("ID3D11VideoDevice", err))?;
    let video_context: ID3D11VideoContext =
        context.cast().map_err(|err| e("ID3D11VideoContext", err))?;

    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
        .map_err(|err| e("MFCreateDXGIDeviceManager", err))?;
    let device_manager = manager.expect("device manager");
    unsafe { device_manager.ResetDevice(&device, reset_token) }
        .map_err(|err| e("ResetDevice", err))?;

    let (swap_chain, frame_latency_waitable) =
        create_swap_chain(&device, hwnd, config.width, config.height)
            .map_err(|err| e("CreateSwapChainForHwnd", err))?;

    // Fail early (before reporting ready) if there's no usable decoder.
    let probe = find_h264_decoder().map_err(|err| e("find decoder MFT", err))?;
    drop(probe);

    if let Some(ready) = ready_tx.take() {
        let _ = ready.send(Ok(()));
    }

    let mut presenter = Presenter {
        device,
        context,
        video_device,
        video_context,
        device_manager,
        swap_chain,
        frame_latency_waitable,
        window_width: config.width,
        window_height: config.height,
        decoder: None,
    };

    loop {
        if pump_messages() {
            // WM_QUIT: the user closed the window.
            return Ok(());
        }
        if shared.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let frame = match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(frame) => frame,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The client dropped the handle.
                let _ = unsafe { DestroyWindow(hwnd) };
                pump_messages();
                return Ok(());
            }
        };
        if frame.generation < shared.newest_generation.load(Ordering::SeqCst) {
            trace(format!(
                "skipping stale generation {} frame {}",
                frame.generation, frame.frame_id
            ));
            continue;
        }

        let dequeue_ts = clock();
        let timing = presenter
            .decode_and_present(frame, dequeue_ts, &clock)
            .map_err(|err| e("decode/present", err))?;
        for t in timing {
            if timing_tx.blocking_send(t).is_err() {
                return Ok(());
            }
        }
    }
}

impl Presenter {
    fn decode_and_present(
        &mut self,
        frame: SubmittedFrame,
        dequeue_ts: u64,
        clock: &Clock,
    ) -> windows::core::Result<Vec<FrameTiming>> {
        let needs_new_decoder = match &self.decoder {
            Some(d) => d.width != frame.width || d.height != frame.height,
            None => true,
        };
        if needs_new_decoder {
            if self.decoder.is_some() {
                eprintln!(
                    "[sardp-win] stream size changed to {}x{}, recreating decoder",
                    frame.width, frame.height
                );
            }
            self.decoder = Some(self.create_decoder(frame.width, frame.height)?);
        }
        // Disjoint borrows: the decoder is mutated per frame while the
        // device-level objects are only read.
        let Presenter {
            video_device,
            video_context,
            swap_chain,
            frame_latency_waitable,
            decoder,
            ..
        } = self;
        let decoder = decoder.as_mut().expect("decoder ensured above");

        let sample = annex_b_sample(&frame.annex_b, frame.receive_ts)?;
        trace(format!(
            "ProcessInput gen={} id={} idr={} {} bytes",
            frame.generation,
            frame.frame_id,
            frame.is_idr,
            frame.annex_b.len()
        ));
        unsafe { decoder.transform.ProcessInput(0, &sample, 0)? };

        let mut timings = Vec::new();
        loop {
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                ..Default::default()
            };
            let mut status = 0u32;
            let result = unsafe {
                decoder
                    .transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            };
            trace(format!(
                "ProcessOutput -> {:?} status={status:#x} sample={}",
                result.as_ref().map_err(|e| e.code()),
                output.pSample.is_some()
            ));
            match result {
                Ok(()) => {}
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The decoder learned the real stream parameters from
                    // the SPS; re-pick the NV12 output type and go again.
                    set_nv12_output_type(&decoder.transform)?;
                    continue;
                }
                Err(err) => return Err(err),
            }
            let Some(decoded) = output.pSample.take() else {
                continue;
            };
            let decode_done_ts = clock();
            decoder.frames_decoded += 1;

            let (texture, subresource) = decoded_texture(&decoded)?;
            // Present only if the swap chain can take it right now;
            // otherwise the frame stays decoded-but-unshown (it may still
            // be a reference for the next one) and the newer frame wins.
            let ready =
                unsafe { WaitForSingleObject(*frame_latency_waitable, 0) } == WAIT_OBJECT_0;
            trace(format!(
                "decoded texture subresource={subresource}, swap chain ready={ready}"
            ));
            if ready {
                present_nv12(video_device, video_context, swap_chain, decoder, &texture, subresource)?;
                trace("presented");
            }
            let display_ts = clock();
            timings.push(FrameTiming {
                generation: frame.generation,
                frame_id: frame.frame_id,
                receive_ts: frame.receive_ts,
                dequeue_ts,
                decode_done_ts,
                display_ts,
                presented: ready,
            });
            // `decoded` (and with it the decoder's surface) is released here.
        }
        Ok(timings)
    }

    fn create_decoder(&self, width: u32, height: u32) -> windows::core::Result<Decoder> {
        let activate = find_h264_decoder()?;
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };

        // Low-latency mode: output each frame as soon as it's decoded.
        // Without it the in-box decoder holds frames back for reordering
        // (it returned NEED_MORE_INPUT for every frame in the first d-3
        // run) even though this stream has no B-frames. Both spellings
        // of the same GUID, since which one a given MFT honors varies.
        unsafe {
            match transform.GetAttributes() {
                Ok(attributes) => {
                    if let Err(err) = attributes.SetUINT32(&MF_LOW_LATENCY, 1) {
                        eprintln!("[sardp-win] decoder MF_LOW_LATENCY rejected: {err}");
                    }
                }
                Err(err) => eprintln!("[sardp-win] decoder has no attribute store: {err}"),
            }
            if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
                // The in-box decoder wants VT_UI4 here, not VT_BOOL.
                let on = VARIANT::from(1u32);
                if let Err(err) = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &on) {
                    eprintln!("[sardp-win] decoder CODECAPI_AVLowLatencyMode rejected: {err}");
                }
            }
        }

        // DXVA: decoded frames as D3D11 NV12 textures on our device.
        unsafe {
            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                Interface::as_raw(&self.device_manager) as usize,
            )?;
        }

        let input_type = unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            t
        };
        unsafe { transform.SetInputType(0, &input_type, 0)? };
        set_nv12_output_type(&transform)?;

        let info = unsafe { transform.GetOutputStreamInfo(0)? };
        if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "decoder MFT does not provide its own (D3D11) output samples; CPU output path not implemented",
            ));
        }

        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)? };
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)? };

        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: self.window_width,
            OutputHeight: self.window_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&content_desc)? };
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0)? };

        Ok(Decoder {
            transform,
            width,
            height,
            processor,
            enumerator,
            frames_decoded: 0,
        })
    }

}

/// NV12 (decoder surface, possibly one slice of a texture array) ->
/// BGRA back buffer, then present.
fn present_nv12(
    video_device: &ID3D11VideoDevice,
    video_context: &ID3D11VideoContext,
    swap_chain: &IDXGISwapChain1,
    decoder: &Decoder,
    texture: &ID3D11Texture2D,
    subresource: u32,
) -> windows::core::Result<()> {
    {
        let src_resource: ID3D11Resource = texture.cast()?;
        let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: subresource,
                },
            },
        };
        let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                &src_resource,
                &decoder.enumerator,
                &input_view_desc,
                Some(&mut input_view),
            )?
        };
        let input_view = input_view.expect("input view");

        let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0)? };
        let dst_resource: ID3D11Resource = back_buffer.cast()?;
        let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &dst_resource,
                &decoder.enumerator,
                &output_view_desc,
                Some(&mut output_view),
            )?
        };
        let output_view = output_view.expect("output view");

        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        }];
        let blt = unsafe {
            video_context.VideoProcessorBlt(&decoder.processor, &output_view, 0, &streams)
        };
        // The struct holds the view as ManuallyDrop (the API takes a raw
        // COM pointer); release it whether or not the blit succeeded.
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        blt?;
    }
    // SyncInterval 0: don't block this thread on vsync. With 1 the thread
    // was capped at one frame per refresh, which equals the capture rate,
    // so a backlog picked up during decoder start-up could never drain
    // (decode latency grew ~16ms per frame in the first d-3 run). Flip
    // model still shows only the most recent frame per refresh.
    unsafe { swap_chain.Present(0, windows::Win32::Graphics::Dxgi::DXGI_PRESENT(0)) }.ok()?;
    Ok(())
}

/// Wraps Annex-B bytes in an IMFSample (system memory; the decoder copies
/// it into its own input buffers).
fn annex_b_sample(annex_b: &[u8], receive_ts: u64) -> windows::core::Result<IMFSample> {
    let len = annex_b.len() as u32;
    let buffer = unsafe { MFCreateMemoryBuffer(len)? };
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(len)?;
    }
    let sample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((receive_ts as i64) * 10)?;
    }
    Ok(sample)
}

/// The D3D11 texture (and array slice) a DXVA-decoded sample lives in.
fn decoded_texture(sample: &IMFSample) -> windows::core::Result<(ID3D11Texture2D, u32)> {
    let buffer = unsafe { sample.GetBufferByIndex(0)? };
    let dxgi: IMFDXGIBuffer = buffer.cast()?;
    let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)? };
    let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
    let subresource = unsafe { dxgi.GetSubresourceIndex()? };
    Ok((texture, subresource))
}

/// Picks the decoder's NV12 output type from what it currently offers
/// (which changes once the SPS has been parsed -- MF_E_TRANSFORM_STREAM_CHANGE).
fn set_nv12_output_type(transform: &IMFTransform) -> windows::core::Result<()> {
    let mut index = 0u32;
    loop {
        // Errors out with MF_E_NO_MORE_TYPES if NV12 isn't offered.
        let candidate: IMFMediaType = unsafe { transform.GetOutputAvailableType(0, index)? };
        let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE)? };
        if subtype == MFVideoFormat_NV12 {
            unsafe { transform.SetOutputType(0, &candidate, 0)? };
            return Ok(());
        }
        index += 1;
    }
}

/// First synchronous H.264 decoder MFT (on Windows this is normally the
/// in-box "Microsoft H264 Video Decoder MFT", which uses DXVA once given a
/// D3D11 device manager). Every enumerated IMFActivate except the kept one
/// is released.
fn find_h264_decoder() -> windows::core::Result<IMFActivate> {
    let input_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input_type),
            None,
            &mut activates,
            &mut count,
        )?;
    }
    if count == 0 {
        unsafe { CoTaskMemFree(Some(activates as *const _)) };
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "no synchronous H.264 decoder MFT found",
        ));
    }
    let slots: &mut [Option<IMFActivate>] =
        unsafe { std::slice::from_raw_parts_mut(activates, count as usize) };
    let mut first: Option<IMFActivate> = None;
    for (i, slot) in slots.iter_mut().enumerate() {
        let owned = slot.take();
        if i == 0 {
            first = owned;
        }
    }
    unsafe { CoTaskMemFree(Some(activates as *const _)) };
    let first = first.expect("first activate present");

    let name = unsafe {
        let mut ptr = windows::core::PWSTR::null();
        let mut len = 0u32;
        match first.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) {
            Ok(()) => {
                let s = ptr.to_string().unwrap_or_default();
                CoTaskMemFree(Some(ptr.0 as *const _));
                s
            }
            Err(_) => "<unknown>".to_string(),
        }
    };
    eprintln!("[sardp-win] using decoder MFT: {name}");
    Ok(first)
}

/// Flip-model swap chain with a frame-latency waitable object (see
/// `Presenter::frame_latency_waitable`). The returned handle must be
/// closed by the caller.
fn create_swap_chain(
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> windows::core::Result<(IDXGISwapChain1, HANDLE)> {
    let dxgi_device: IDXGIDevice = device.cast()?;
    let adapter = unsafe { dxgi_device.GetAdapter()? };
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
    };
    let swap_chain = unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None)? };
    let swap_chain2: IDXGISwapChain2 = swap_chain.cast()?;
    unsafe { swap_chain2.SetMaximumFrameLatency(1)? };
    let waitable = unsafe { swap_chain2.GetFrameLatencyWaitableObject() };
    if waitable.is_invalid() {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "GetFrameLatencyWaitableObject returned an invalid handle",
        ));
    }
    Ok((swap_chain, waitable))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    if !state.is_null() {
        let state = unsafe { &mut *state };
        if let Some(result) = handle_input_message(hwnd, state, msg, wparam, lparam) {
            return result;
        }
    }
    match msg {
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Whether the message being processed came from this machine's own
/// `sardp-win` injector (server and client on one machine): forwarding it
/// again would echo forever.
fn is_injected_echo() -> bool {
    unsafe { GetMessageExtraInfo() }.0 as usize == INJECTED_EXTRA_INFO
}

fn current_modifiers() -> u16 {
    let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| {
        let state = unsafe { GetKeyState(i32::from(vk.0)) };
        state < 0
    };
    let mut modifiers = 0;
    if down(VK_SHIFT) {
        modifiers |= modifier::SHIFT;
    }
    if down(VK_CONTROL) {
        modifiers |= modifier::CTRL;
    }
    if down(VK_MENU) {
        modifiers |= modifier::ALT;
    }
    if down(VK_LWIN) || down(VK_RWIN) {
        modifiers |= modifier::META;
    }
    modifiers
}

fn mouse_position(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16;
    (i32::from(x), i32::from(y))
}

/// Input-related messages; `None` hands the message to the default
/// window procedure.
fn handle_input_message(
    hwnd: HWND,
    state: &mut WindowState,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let send = |state: &WindowState, event: WindowInput| {
        // The receiver is gone only while the client is shutting down.
        let _ = state.input_tx.send(event);
    };
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            // Consumed either way: no local menu activation for Alt/F10,
            // and Alt+F4 goes to the remote desktop, not this window.
            if is_injected_echo() || state.composing {
                return Some(LRESULT(0));
            }
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let l = lparam.0 as u32;
            let scancode = ((l >> 16) & 0xFF) as u16;
            let extended = (l >> 24) & 1 == 1;
            match keymap::scancode_to_hid(scancode, extended) {
                Some(hid_usage) => send(
                    state,
                    WindowInput::Key {
                        down,
                        hid_usage,
                        virtual_key: wparam.0 as u32,
                        modifiers: current_modifiers(),
                    },
                ),
                None => trace(format!(
                    "unmapped key: scancode={scancode:#x} extended={extended} vk={:#x}",
                    wparam.0
                )),
            }
            Some(LRESULT(0))
        }
        WM_CHAR => {
            if is_injected_echo() {
                return Some(LRESULT(0));
            }
            let unit = wparam.0 as u16;
            if (0xD800..=0xDBFF).contains(&unit) {
                state.pending_high_surrogate = Some(unit);
                return Some(LRESULT(0));
            }
            let text = match state.pending_high_surrogate.take() {
                Some(high) if (0xDC00..=0xDFFF).contains(&unit) => {
                    String::from_utf16_lossy(&[high, unit])
                }
                _ => match char::from_u32(u32::from(unit)) {
                    Some(c) => c.to_string(),
                    None => return Some(LRESULT(0)),
                },
            };
            // Enter/Tab/Backspace/Escape and Ctrl+letter arrive here as
            // control characters; those keys travel as `Key` events.
            if text.chars().all(char::is_control) {
                return Some(LRESULT(0));
            }
            send(state, WindowInput::Text(text));
            Some(LRESULT(0))
        }
        // Alt+key: the `Key` event carries it; don't let DefWindowProc
        // treat it as a menu mnemonic.
        WM_SYSCHAR => Some(LRESULT(0)),
        WM_IME_STARTCOMPOSITION => {
            state.composing = true;
            None
        }
        WM_IME_ENDCOMPOSITION => {
            state.composing = false;
            send(
                state,
                WindowInput::ImeComposition {
                    text: String::new(),
                    caret: 0,
                },
            );
            None
        }
        WM_IME_COMPOSITION => {
            if (lparam.0 as u32) & GCS_COMPSTR.0 != 0
                && let Some((text, caret)) = read_composition(hwnd)
            {
                send(state, WindowInput::ImeComposition { text, caret });
            }
            // DefWindowProc turns the result string into WM_CHARs.
            None
        }
        WM_MOUSEMOVE => {
            if !is_injected_echo() {
                let (x, y) = mouse_position(lparam);
                send(state, WindowInput::MouseMove { x, y });
            }
            Some(LRESULT(0))
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (button, down) = match msg {
                WM_LBUTTONDOWN => (button::LEFT, true),
                WM_LBUTTONUP => (button::LEFT, false),
                WM_RBUTTONDOWN => (button::RIGHT, true),
                WM_RBUTTONUP => (button::RIGHT, false),
                WM_MBUTTONDOWN => (button::MIDDLE, true),
                WM_MBUTTONUP => (button::MIDDLE, false),
                _ => {
                    let which = ((wparam.0 >> 16) & 0xFFFF) as u16;
                    let button = if which == 2 { button::X2 } else { button::X1 };
                    (button, msg == WM_XBUTTONDOWN)
                }
            };
            // Keep receiving the drag even when it leaves the window.
            if down {
                state.buttons_down = state.buttons_down.saturating_add(1);
                unsafe { SetCapture(hwnd) };
            } else {
                state.buttons_down = state.buttons_down.saturating_sub(1);
                if state.buttons_down == 0 {
                    let _ = unsafe { ReleaseCapture() };
                }
            }
            if !is_injected_echo() {
                let (x, y) = mouse_position(lparam);
                send(state, WindowInput::MouseButton { button, down, x, y });
            }
            Some(LRESULT(0))
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if !is_injected_echo() {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let vertical = msg == WM_MOUSEWHEEL;
                let (dx, dy) = if vertical { (0, delta) } else { (delta, 0) };
                send(state, WindowInput::Wheel { dx, dy });
            }
            Some(LRESULT(0))
        }
        WM_KILLFOCUS => {
            state.composing = false;
            send(state, WindowInput::FocusLost);
            None
        }
        _ => None,
    }
}

/// The in-progress composition string and caret from the window's IME
/// context.
fn read_composition(hwnd: HWND) -> Option<(String, u16)> {
    let himc: HIMC = unsafe { ImmGetContext(hwnd) };
    if himc == HIMC::default() {
        return None;
    }
    let result = (|| {
        let bytes = unsafe { ImmGetCompositionStringW(himc, GCS_COMPSTR, None, 0) };
        if bytes < 0 {
            return None;
        }
        let mut buf = vec![0u16; bytes as usize / 2];
        if !buf.is_empty() {
            let written = unsafe {
                ImmGetCompositionStringW(
                    himc,
                    GCS_COMPSTR,
                    Some(buf.as_mut_ptr().cast()),
                    bytes as u32,
                )
            };
            if written < 0 {
                return None;
            }
            buf.truncate(written as usize / 2);
        }
        let caret = unsafe { ImmGetCompositionStringW(himc, GCS_CURSORPOS, None, 0) }.max(0);
        Some((String::from_utf16_lossy(&buf), caret.min(i32::from(u16::MAX)) as u16))
    })();
    let _ = unsafe { ImmReleaseContext(hwnd, himc) };
    result
}

fn create_window(config: &DisplayConfig) -> windows::core::Result<HWND> {
    let hinstance = unsafe { GetModuleHandleW(None)? };
    let class_name = wide("SardpDisplayWindow");
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // Returns 0 if the class already exists (a second window in the same
    // process); that's fine, CreateWindowExW below just uses it.
    let _ = unsafe { RegisterClassW(&class) };

    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: config.width as i32,
        bottom: config.height as i32,
    };
    unsafe { AdjustWindowRect(&mut rect, style, false)? };
    let title = wide(&config.title);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LEFT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    Ok(hwnd)
}

/// Drains the thread's message queue. Returns `true` on WM_QUIT.
fn pump_messages() -> bool {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return true;
            }
            // An echo of our own injector's key press must not be
            // translated into a WM_CHAR either (it would be forwarded as
            // text and injected again).
            let injected_key =
                matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN) && is_injected_echo();
            if !injected_key {
                let _ = TranslateMessage(&msg);
            }
            DispatchMessageW(&msg);
        }
    }
    false
}
