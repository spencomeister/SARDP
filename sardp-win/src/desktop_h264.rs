//! Desktop capture + hardware H.264 encode as a frame *source*.
//!
//! [`DesktopH264Source::start`] spawns a dedicated OS thread that owns every
//! COM object (D3D11 device, DXGI duplication, video processor, encoder
//! MFT) for its whole lifetime and pushes finished [`EncodedFrame`]s into
//! a bounded `tokio::sync::mpsc` channel. Keeping all COM state on one
//! thread sidesteps apartment/threading questions entirely, and the bounded
//! channel gives source-side drop under backpressure (DR-007): if the
//! consumer falls behind, the newest frame is dropped rather than queued.
//!
//! Ported from `tools/dxgi-capture-poc/src/bin/mf_h264_encode.rs` (3W-1-b);
//! see that file and its README for the MFT quirks this code works around
//! (NV12-only input, `MF_TRANSFORM_ASYNC_UNLOCK`, blocking `GetEvent` never
//! returning, drain completion not being signalled).

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use windows::core::{Interface, PWSTR};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFDXGIDeviceManager, IMFMediaEvent, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFTransform, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFShutdown, MFStartup, MFTEnumEx, MFMediaType_Video,
    MFSampleExtension_CleanPoint, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
    METransformHaveOutput, METransformNeedInput, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFSTARTUP_FULL,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use dxgi_capture_poc::capture::{
    create_d3d11_device, create_output_duplication, primary_display_refresh_interval, FrameGuard,
};

/// Monotonic microsecond clock supplied by the caller, so the frames'
/// `capture_ts`/`encode_done_ts` share the server's `VideoFrameHeader`
/// clock basis (this crate deliberately doesn't depend on `sardp`).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct DesktopH264Config {
    pub bitrate_bps: u32,
    /// Encode every frame as an IDR (GOP size 1). Costs bitrate but keeps
    /// each frame independently decodable -- needed while the client still
    /// decodes each frame with a fresh `ffmpeg` process (3W-1-d-2; the
    /// persistent decoder is 3W-1-d-3).
    pub all_idr: bool,
    /// `AcquireNextFrame` timeout; also bounds how often the worker checks
    /// its stop flag.
    pub acquire_timeout: Duration,
}

impl Default for DesktopH264Config {
    fn default() -> Self {
        Self {
            bitrate_bps: 8_000_000,
            all_idr: true,
            acquire_timeout: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceInfo {
    pub width: u32,
    pub height: u32,
    /// Display refresh rate, rounded; also the encoder's nominal frame rate.
    pub fps: u32,
}

/// One encoded frame: an Annex-B H.264 access unit. When `is_idr`, the
/// bytes are self-contained (SPS+PPS precede the IDR slice, spec 2.10).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub annex_b: Vec<u8>,
    pub is_idr: bool,
    pub capture_ts: u64,
    pub encode_done_ts: u64,
}

#[derive(Debug)]
pub enum WinCaptureError {
    /// The worker thread failed to set up capture/encode.
    Init(String),
    /// The worker thread died after startup (e.g. `DXGI_ERROR_ACCESS_LOST`).
    Worker(String),
}

impl std::fmt::Display for WinCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(s) => write!(f, "desktop capture init failed: {s}"),
            Self::Worker(s) => write!(f, "desktop capture worker failed: {s}"),
        }
    }
}

/// How many encoded frames may wait for the consumer before the worker
/// starts dropping (source-side drop, DR-007).
const CHANNEL_CAPACITY: usize = 4;

/// A running capture+encode pipeline. Dropping it stops the worker thread.
pub struct DesktopH264Source {
    rx: mpsc::Receiver<EncodedFrame>,
    info: SourceInfo,
    stop: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DesktopH264Source {
    /// Starts the worker and blocks (briefly) until it has captured its
    /// first frame and therefore knows the display dimensions.
    pub fn start(config: DesktopH264Config, clock: Clock) -> Result<Self, WinCaptureError> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<SourceInfo, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let force_idr = Arc::new(AtomicBool::new(false));

        let worker = {
            let stop = stop.clone();
            let force_idr = force_idr.clone();
            std::thread::Builder::new()
                .name("sardp-win-capture".into())
                .spawn(move || worker_main(config, clock, tx, ready_tx, stop, force_idr))
                .map_err(|e| WinCaptureError::Init(format!("spawn worker thread: {e}")))?
        };

        let info = match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = worker.join();
                return Err(WinCaptureError::Init(e));
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                return Err(WinCaptureError::Init(
                    "worker did not produce a first frame within 15s".into(),
                ));
            }
        };

        Ok(Self {
            rx,
            info,
            stop,
            force_idr,
            worker: Some(worker),
        })
    }

    pub fn info(&self) -> SourceInfo {
        self.info
    }

    /// Next encoded frame, or `None` once the worker has stopped (an error
    /// after startup surfaces as the channel closing; the reason is logged
    /// by the worker).
    pub async fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.rx.recv().await
    }

    /// Ask the encoder to make the next frame an IDR (needed when a new
    /// video Instance/generation is opened and the stream must restart from
    /// a self-contained frame). A no-op in `all_idr` mode.
    pub fn request_idr(&self) {
        self.force_idr.store(true, Ordering::SeqCst);
    }
}

impl Drop for DesktopH264Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_main(
    config: DesktopH264Config,
    clock: Clock,
    tx: mpsc::Sender<EncodedFrame>,
    ready_tx: std::sync::mpsc::Sender<Result<SourceInfo, String>>,
    stop: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
) {
    // COM/MF lifetime brackets the whole worker; every COM object is created
    // and dropped inside `run_worker` so MFShutdown runs after them.
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        let _ = ready_tx.send(Err(format!("MFStartup: {e}")));
        if com_ok {
            unsafe { CoUninitialize() };
        }
        return;
    }

    let mut ready_tx = Some(ready_tx);
    if let Err(e) = run_worker(config, clock, &tx, &mut ready_tx, &stop, &force_idr) {
        // If we never reported readiness, this is an init failure; otherwise
        // the consumer just sees the channel close.
        if let Some(ready) = ready_tx.take() {
            let _ = ready.send(Err(e));
        } else {
            eprintln!("[sardp-win] capture worker stopped: {e}");
        }
    }

    unsafe {
        let _ = MFShutdown();
        if com_ok {
            CoUninitialize();
        }
    }
}

fn run_worker(
    config: DesktopH264Config,
    clock: Clock,
    tx: &mpsc::Sender<EncodedFrame>,
    ready_tx: &mut Option<std::sync::mpsc::Sender<Result<SourceInfo, String>>>,
    stop: &AtomicBool,
    force_idr: &AtomicBool,
) -> Result<(), String> {
    let e = |ctx: &str, err: windows::core::Error| format!("{ctx}: {err}");

    let (device, context) = create_d3d11_device(
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    )
    .map_err(|err| e("D3D11CreateDevice", err))?;
    let multithread: ID3D11Multithread = device.cast().map_err(|err| e("ID3D11Multithread", err))?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let device_manager = create_device_manager(&device).map_err(|err| e("device manager", err))?;
    let duplication = create_output_duplication(&device).map_err(|err| e("DuplicateOutput", err))?;

    let refresh = primary_display_refresh_interval();
    let fps = (1.0 / refresh.as_secs_f64()).round().max(1.0) as u32;
    let acquire_timeout_ms = config.acquire_timeout.as_millis().min(u32::MAX as u128) as u32;

    let mut converter: Option<VideoConverter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut dropped: u64 = 0;

    while !stop.load(Ordering::SeqCst) {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            duplication.AcquireNextFrame(acquire_timeout_ms, &mut frame_info, &mut resource)
        };
        match acquired {
            Ok(()) => {}
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                return Err(e("AcquireNextFrame (access lost; display mode change/lock screen?)", err));
            }
            Err(err) => return Err(e("AcquireNextFrame", err)),
        }
        let capture_ts = clock();

        // Same RAII guard as 3W-1-a: ReleaseFrame on every exit path.
        let frame_guard = FrameGuard {
            duplication: &duplication,
        };
        let resource = resource.expect("AcquireNextFrame succeeded without a resource");
        let texture: ID3D11Texture2D = resource.cast().map_err(|err| e("frame texture cast", err))?;

        if encoder.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            converter = Some(
                VideoConverter::new(&device, &context, desc.Width, desc.Height, fps)
                    .map_err(|err| e("VideoConverter", err))?,
            );
            encoder = Some(
                Encoder::new(&device_manager, desc.Width, desc.Height, fps, &config)
                    .map_err(|err| e("Encoder", err))?,
            );
            if let Some(ready) = ready_tx.take() {
                let _ = ready.send(Ok(SourceInfo {
                    width: desc.Width,
                    height: desc.Height,
                    fps,
                }));
            }
        }

        let nv12 = converter
            .as_ref()
            .expect("converter initialized above")
            .convert(&context, &texture)
            .map_err(|err| e("BGRA->NV12", err))?;
        // The copy into our own texture has been issued; the DXGI frame can
        // go back now (before the encoder does its work).
        drop(frame_guard);

        let enc = encoder.as_mut().expect("encoder initialized above");
        if force_idr.swap(false, Ordering::SeqCst) {
            enc.force_next_idr();
        }
        let outputs = enc
            .encode_frame(nv12, capture_ts, &clock)
            .map_err(|err| e("encode", err))?;
        for frame in outputs {
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    dropped += 1;
                    if dropped.is_power_of_two() {
                        eprintln!("[sardp-win] consumer behind; dropped {dropped} encoded frame(s) so far");
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
            }
        }
    }

    if let Some(mut enc) = encoder {
        let _ = enc.finish();
    }
    Ok(())
}

fn create_device_manager(device: &ID3D11Device) -> windows::core::Result<IMFDXGIDeviceManager> {
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
    let manager = manager.expect("device manager");
    unsafe { manager.ResetDevice(device, reset_token)? };
    Ok(manager)
}

/// `MFSetAttributeSize`/`MFSetAttributeRatio` equivalent (header-only inline
/// helpers in mfapi.h, absent from the metadata): high<<32 | low.
fn set_attribute_u64_pair(
    attrs: &IMFMediaType,
    key: &windows::core::GUID,
    high: u32,
    low: u32,
) -> windows::core::Result<()> {
    unsafe { attrs.SetUINT64(key, ((high as u64) << 32) | (low as u64)) }
}

/// GPU-side BGRA -> NV12 (the only input the hardware encoder MFT accepts
/// on the validated machine). Two textures allocated once and reused.
struct VideoConverter {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    bgra: ID3D11Texture2D,
    nv12: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
}

impl VideoConverter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
    ) -> windows::core::Result<Self> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;

        let bgra_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut bgra: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&bgra_desc, None, Some(&mut bgra))? };
        let bgra = bgra.expect("bgra texture");

        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Format: DXGI_FORMAT_NV12,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..bgra_desc
        };
        let mut nv12: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12))? };
        let nv12 = nv12.expect("nv12 texture");

        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        let src_resource: ID3D11Resource = bgra.cast()?;
        let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                &src_resource,
                &enumerator,
                &input_view_desc,
                Some(&mut input_view),
            )?
        };
        let input_view = input_view.expect("input view");

        let dst_resource: ID3D11Resource = nv12.cast()?;
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
                &enumerator,
                &output_view_desc,
                Some(&mut output_view),
            )?
        };
        let output_view = output_view.expect("output view");

        Ok(Self {
            video_context,
            processor,
            bgra,
            nv12,
            input_view,
            output_view,
        })
    }

    fn convert(
        &self,
        context: &ID3D11DeviceContext,
        src_frame: &ID3D11Texture2D,
    ) -> windows::core::Result<&ID3D11Texture2D> {
        let src: ID3D11Resource = src_frame.cast()?;
        let dst: ID3D11Resource = self.bgra.cast()?;
        unsafe { context.CopyResource(&dst, &src) };

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, &[stream])?;
        }
        Ok(&self.nv12)
    }
}

/// The hardware H.264 encoder MFT, driven directly. Encoded samples come
/// back as Annex-B byte buffers (no muxing).
struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    all_idr: bool,
    fps: u32,
    /// SPS+PPS (Annex-B), prepended to any IDR sample the encoder emits
    /// without its own parameter sets (spec 2.10: every IDR MUST be
    /// self-contained). Taken from the first sample that carries them
    /// (hardware MFTs typically only include them with the very first IDR),
    /// with `MF_MT_MPEG_SEQUENCE_HEADER` on the negotiated output type as a
    /// fallback source.
    sequence_header: Option<Vec<u8>>,
    /// `capture_ts` of inputs not yet matched to an output (1:1, no B-frames).
    pending_capture_ts: VecDeque<u64>,
    last_pts_100ns: Option<i64>,
    input_count: u64,
    output_count: u64,
    finished: bool,
}

impl Encoder {
    fn new(
        device_manager: &IMFDXGIDeviceManager,
        width: u32,
        height: u32,
        fps: u32,
        config: &DesktopH264Config,
    ) -> windows::core::Result<Self> {
        let activate = find_hardware_h264_encoder()?;
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };

        // Async MFTs refuse every call until unlocked.
        unsafe {
            transform
                .GetAttributes()?
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        }
        unsafe {
            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                Interface::as_raw(device_manager) as usize,
            )?;
        }

        // Codec API is optional (a vendor MFT may not expose it). GOP size
        // must be set before the media types are negotiated -- set
        // afterwards it was silently ignored on the validated machine
        // (3W-1-d-2 smoke test: P-frames kept coming). IDR-ness of each
        // output is verified from the NAL units regardless, so a
        // non-cooperating encoder degrades to "some frames aren't IDR",
        // not to mislabelled frames.
        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if config.all_idr {
            if let Some(api) = &codec_api {
                let gop = VARIANT::from(1u32);
                if let Err(err) = unsafe { api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop) } {
                    eprintln!("[sardp-win] CODECAPI_AVEncMPVGOPSize=1 rejected: {err}");
                }
            } else {
                eprintln!("[sardp-win] encoder MFT has no ICodecAPI; cannot force all-IDR");
            }
        }

        let output_type = unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            set_attribute_u64_pair(&t, &MF_MT_FRAME_SIZE, width, height)?;
            set_attribute_u64_pair(&t, &MF_MT_FRAME_RATE, fps, 1)?;
            set_attribute_u64_pair(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            t
        };
        unsafe { transform.SetOutputType(0, &output_type, 0)? };

        let input_type = unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            set_attribute_u64_pair(&t, &MF_MT_FRAME_SIZE, width, height)?;
            set_attribute_u64_pair(&t, &MF_MT_FRAME_RATE, fps, 1)?;
            set_attribute_u64_pair(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            t
        };
        unsafe { transform.SetInputType(0, &input_type, 0)? };

        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)? };
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)? };
        let events: IMFMediaEventGenerator = transform.cast()?;

        Ok(Self {
            transform,
            events,
            codec_api,
            all_idr: config.all_idr,
            fps,
            sequence_header: None,
            pending_capture_ts: VecDeque::new(),
            last_pts_100ns: None,
            input_count: 0,
            output_count: 0,
            finished: false,
        })
    }

    fn force_next_idr(&mut self) {
        if let Some(api) = &self.codec_api {
            let one = VARIANT::from(1u32);
            if let Err(err) = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &one) } {
                eprintln!("[sardp-win] CODECAPI_AVEncVideoForceKeyFrame rejected: {err}");
            }
        }
    }

    /// Feeds one NV12 texture and returns whatever encoded frames came out
    /// (usually the one for this input; possibly 0 or several with an
    /// encoder that pipelines).
    fn encode_frame(
        &mut self,
        texture: &ID3D11Texture2D,
        capture_ts: u64,
        clock: &Clock,
    ) -> windows::core::Result<Vec<EncodedFrame>> {
        // Sample time: 100ns units from the caller's clock; duration from
        // the gap to the previous input (nominal 1/fps for the first).
        let pts = (capture_ts as i64) * 10;
        let duration = match self.last_pts_100ns {
            Some(prev) => (pts - prev).max(1),
            None => 10_000_000 / self.fps as i64,
        };
        self.last_pts_100ns = Some(pts);

        let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)? };
        let sample: IMFSample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(duration)?;
        }

        let mut outputs = Vec::new();
        loop {
            let event = self.wait_for_event(Duration::from_secs(5))?;
            let event_type = unsafe { event.GetType()? };
            if event_type == METransformNeedInput.0 as u32 {
                if self.all_idr {
                    // Belt and braces with the GOP-size setting: some MFTs
                    // honor one but not the other.
                    self.force_next_idr();
                }
                unsafe { self.transform.ProcessInput(0, &sample, 0)? };
                self.input_count += 1;
                self.pending_capture_ts.push_back(capture_ts);
                break;
            } else if event_type == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.take_one_output(clock)? {
                    outputs.push(frame);
                }
            }
        }
        // Anything already ready (typically this frame's own output).
        loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let event_type = unsafe { event.GetType()? };
                    if event_type == METransformHaveOutput.0 as u32 {
                        if let Some(frame) = self.take_one_output(clock)? {
                            outputs.push(frame);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        Ok(outputs)
    }

    /// Non-blocking poll for events with a timeout: on this MFT a blocking
    /// `GetEvent` never returns (3W-1-b finding).
    fn wait_for_event(&self, timeout: Duration) -> windows::core::Result<IMFMediaEvent> {
        let start = Instant::now();
        loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => return Ok(event),
                Err(_) => {
                    if start.elapsed() > timeout {
                        return Err(windows::core::Error::new(
                            windows::Win32::Foundation::E_FAIL,
                            "timed out waiting for MFT event",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    fn take_one_output(&mut self, clock: &Clock) -> windows::core::Result<Option<EncodedFrame>> {
        let mut output_buffer = MFT_OUTPUT_DATA_BUFFER::default();
        output_buffer.dwStreamID = 0;
        let mut status = 0u32;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output_buffer), &mut status)
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(err) => return Err(err),
        }
        let Some(sample) = output_buffer.pSample.take() else {
            return Ok(None);
        };
        self.output_count += 1;
        let encode_done_ts = clock();
        let capture_ts = self.pending_capture_ts.pop_front().unwrap_or(encode_done_ts);

        let mut annex_b = sample_bytes(&sample)?;
        // MFSampleExtension_CleanPoint proved unreliable on the validated
        // MFT (set on the first IDR only), so look at the NAL units too.
        let clean_point =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let nal_types = nal_unit_types(&annex_b);
        let is_idr = clean_point || nal_types.contains(&5);
        if self.sequence_header.is_none() {
            if nal_types.contains(&7) && nal_types.contains(&8) {
                self.sequence_header = extract_parameter_sets(&annex_b);
            } else {
                self.sequence_header = self.read_sequence_header();
            }
        }
        if is_idr && !nal_types.contains(&7) {
            if let Some(header) = &self.sequence_header {
                let mut with_header = Vec::with_capacity(header.len() + annex_b.len());
                with_header.extend_from_slice(header);
                with_header.append(&mut annex_b);
                annex_b = with_header;
            }
        }
        Ok(Some(EncodedFrame {
            annex_b,
            is_idr,
            capture_ts,
            encode_done_ts,
        }))
    }

    fn read_sequence_header(&self) -> Option<Vec<u8>> {
        let media_type = unsafe { self.transform.GetOutputCurrentType(0) }.ok()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut size = 0u32;
        unsafe { media_type.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut size) }
            .ok()?;
        if ptr.is_null() || size == 0 {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, size as usize) }.to_vec();
        unsafe { CoTaskMemFree(Some(ptr as *const _)) };
        Some(bytes)
    }

    fn finish(&mut self) -> windows::core::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
        // Nobody consumes drained output at shutdown; just bound the wait.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.output_count < self.input_count && Instant::now() < deadline {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    if unsafe { event.GetType()? } == METransformHaveOutput.0 as u32 {
                        let clock: Clock = Arc::new(|| 0);
                        let _ = self.take_one_output(&clock)?;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Copies an encoded sample's bytes out (all its buffers, contiguous).
fn sample_bytes(sample: &IMFSample) -> windows::core::Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut len))? };
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
    unsafe { buffer.Unlock()? };
    Ok(bytes)
}

/// Byte offsets at which Annex-B start codes (`00 00 01` / `00 00 00 01`)
/// begin, paired with the offset of the NAL header byte that follows. A
/// minimal scanner (the full splitter lives in `sardp::h264`, which this
/// crate can't depend on).
fn nal_starts(annex_b: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 {
            if annex_b[i + 2] == 1 {
                out.push((i, i + 3));
                i += 3;
                continue;
            }
            if annex_b[i + 2] == 0 && annex_b.get(i + 3) == Some(&1) {
                out.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `nal_unit_type` of every NAL unit, in order.
fn nal_unit_types(annex_b: &[u8]) -> Vec<u8> {
    nal_starts(annex_b)
        .into_iter()
        .filter_map(|(_, header)| annex_b.get(header).map(|b| b & 0x1F))
        .collect()
}

/// The SPS (7) and PPS (8) NAL units, start codes included, concatenated
/// in stream order -- i.e. exactly what has to precede an IDR slice for
/// the access unit to be self-contained.
fn extract_parameter_sets(annex_b: &[u8]) -> Option<Vec<u8>> {
    let starts = nal_starts(annex_b);
    let mut out = Vec::new();
    for (idx, &(start, header)) in starts.iter().enumerate() {
        let nal_type = annex_b.get(header).map(|b| b & 0x1F)?;
        if nal_type == 7 || nal_type == 8 {
            let end = starts.get(idx + 1).map(|&(s, _)| s).unwrap_or(annex_b.len());
            out.extend_from_slice(&annex_b[start..end]);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// First hardware H.264 encoder MFT (vendor-neutral; on the validated
/// machine this is "NVIDIA H.264 Encoder MFT"). Every IMFActivate the
/// enumeration returns is released except the one kept (3W-1-b review).
fn find_hardware_h264_encoder() -> windows::core::Result<IMFActivate> {
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER | MFT_ENUM_FLAG_SYNCMFT,
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
    }
    if count == 0 {
        unsafe { CoTaskMemFree(Some(activates as *const _)) };
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "no hardware H.264 encoder MFT found",
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
        let mut ptr = PWSTR::null();
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
    eprintln!("[sardp-win] using hardware encoder MFT: {name}");
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::{extract_parameter_sets, nal_unit_types};

    #[test]
    fn nal_unit_types_handles_both_start_code_lengths() {
        let buf = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC];
        assert_eq!(nal_unit_types(&buf), vec![7, 8, 5]);
        assert_eq!(nal_unit_types(&[0, 0, 0, 0]), Vec::<u8>::new());
        assert_eq!(nal_unit_types(&[]), Vec::<u8>::new());
    }

    #[test]
    fn extract_parameter_sets_keeps_sps_and_pps_with_their_start_codes() {
        let sps = [0, 0, 0, 1, 0x67, 0xAA, 0xAB];
        let pps = [0, 0, 1, 0x68, 0xBB];
        let idr = [0, 0, 0, 1, 0x65, 0xCC, 0xCD];
        let mut buf = Vec::new();
        buf.extend_from_slice(&sps);
        buf.extend_from_slice(&pps);
        buf.extend_from_slice(&idr);
        let mut expected = Vec::new();
        expected.extend_from_slice(&sps);
        expected.extend_from_slice(&pps);
        assert_eq!(extract_parameter_sets(&buf), Some(expected));
        assert_eq!(extract_parameter_sets(&idr), None);
    }
}
