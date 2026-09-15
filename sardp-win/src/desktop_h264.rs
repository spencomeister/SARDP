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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sardp::frame_source::{
    Clock, DEFAULT_CHANNEL_CAPACITY, DesktopH264Config, EncodedFrame, FrameWorker, SendOutcome,
    SourceError, SourceInfo, WorkerContext,
};
use sardp::h264::{self, ParameterSetCache};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIResource,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEvent, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformHaveOutput, METransformNeedInput, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Video,
    MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFShutdown, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, PWSTR};

use dxgi_capture_poc::capture::{
    create_d3d11_device, create_output_duplication, duplicated_output_desktop_rect,
    primary_display_refresh_interval, release_frame_on_drop,
};

/// Kept for callers written against the 3W-1-d-2 API; the type itself now
/// lives in `sardp::frame_source` (shared with the other platforms).
pub type WinCaptureError = SourceError;

/// A running capture+encode pipeline. Dropping it stops the worker thread.
///
/// The thread/channel/readiness mechanics are `sardp::frame_source`'s
/// [`FrameWorker`] (shared with every platform); this type adds the one
/// Windows-side control the encoder needs, [`Self::request_idr`].
pub struct DesktopH264Source {
    worker: FrameWorker<EncodedFrame, SourceInfo>,
    force_idr: Arc<AtomicBool>,
}

impl DesktopH264Source {
    /// Starts the worker and blocks (briefly) until it has captured its
    /// first frame and therefore knows the display dimensions.
    pub fn start(config: DesktopH264Config, clock: Clock) -> Result<Self, SourceError> {
        let force_idr = Arc::new(AtomicBool::new(false));
        let worker = {
            let force_idr = force_idr.clone();
            FrameWorker::spawn(
                "sardp-win-capture",
                DEFAULT_CHANNEL_CAPACITY,
                Duration::from_secs(15),
                move |ctx| worker_main(config, clock, ctx, &force_idr),
            )?
        };
        Ok(Self { worker, force_idr })
    }

    pub fn info(&self) -> SourceInfo {
        *self.worker.info()
    }

    /// Next encoded frame, or `None` once the worker has stopped (an error
    /// after startup surfaces as the channel closing; the reason is logged
    /// by the worker).
    pub async fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.worker.next().await
    }

    /// Ask the encoder to make the next frame an IDR (needed when a new
    /// video Instance/generation is opened and the stream must restart from
    /// a self-contained frame). A no-op in `all_idr` mode.
    pub fn request_idr(&self) {
        self.force_idr.store(true, Ordering::SeqCst);
    }
}

fn worker_main(
    config: DesktopH264Config,
    clock: Clock,
    ctx: &mut WorkerContext<EncodedFrame, SourceInfo>,
    force_idr: &AtomicBool,
) -> Result<(), String> {
    // COM/MF lifetime brackets the whole worker; every COM object is created
    // and dropped inside `run_worker` so MFShutdown runs after them.
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        if com_ok {
            unsafe { CoUninitialize() };
        }
        return Err(format!("MFStartup: {e}"));
    }

    let result = run_worker(config, clock, ctx, force_idr);

    unsafe {
        let _ = MFShutdown();
        if com_ok {
            CoUninitialize();
        }
    }
    result
}

fn run_worker(
    config: DesktopH264Config,
    clock: Clock,
    ctx: &mut WorkerContext<EncodedFrame, SourceInfo>,
    force_idr: &AtomicBool,
) -> Result<(), String> {
    let e = |ctx: &str, err: windows::core::Error| format!("{ctx}: {err}");

    let (device, context) =
        create_d3d11_device(D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT)
            .map_err(|err| e("D3D11CreateDevice", err))?;
    let multithread: ID3D11Multithread =
        device.cast().map_err(|err| e("ID3D11Multithread", err))?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let device_manager = create_device_manager(&device).map_err(|err| e("device manager", err))?;
    let duplication =
        create_output_duplication(&device).map_err(|err| e("DuplicateOutput", err))?;
    let origin = match duplicated_output_desktop_rect(&device) {
        Ok(rect) => (rect.left, rect.top),
        Err(err) => {
            eprintln!("[sardp-win] output desktop rect unavailable ({err}); assuming origin (0,0)");
            (0, 0)
        }
    };

    let refresh = primary_display_refresh_interval();
    let fps = (1.0 / refresh.as_secs_f64()).round().max(1.0) as u32;
    let acquire_timeout_ms = config.acquire_timeout.as_millis().min(u32::MAX as u128) as u32;

    let mut converter: Option<VideoConverter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut stats = CaptureStats::default();
    // Pacing: at most one encoded frame per display refresh. DXGI hands
    // out more "frames" than that -- pointer-only updates carry no new
    // desktop image at all (`LastPresentTime == 0`), and the first d-3 run
    // measured ~120 frames/s reaching the client from a 59Hz display,
    // which is what overloaded its decoder. The negotiated
    // `EncoderConfig.max_fps` is `fps`, so the source must honor it.
    let min_interval = refresh;
    let mut next_due: Option<Instant> = None;
    let start = Instant::now();
    let mut warned_no_image = false;

    while !ctx.should_stop() {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            duplication.AcquireNextFrame(acquire_timeout_ms, &mut frame_info, &mut resource)
        };
        match acquired {
            Ok(()) => {}
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                return Err(e(
                    "AcquireNextFrame (access lost; display mode change/lock screen?)",
                    err,
                ));
            }
            Err(err) => return Err(e("AcquireNextFrame", err)),
        }
        let capture_ts = clock();
        let now = Instant::now();

        // Same RAII guard as 3W-1-a: ReleaseFrame on every exit path.
        let frame_guard = release_frame_on_drop(&duplication);
        stats.acquired += 1;
        if stats.acquired <= 3 {
            eprintln!(
                "[sardp-win] capture frame {}: LastPresentTime={} AccumulatedFrames={} metadata={}B rects_coalesced={} protected={}",
                stats.acquired,
                frame_info.LastPresentTime,
                frame_info.AccumulatedFrames,
                frame_info.TotalMetadataBufferSize,
                frame_info.RectsCoalesced.as_bool(),
                frame_info.ProtectedContentMaskedOut.as_bool(),
            );
        }
        // `LastPresentTime == 0` means no new desktop image in this frame
        // (pointer-only update) -- and that includes the very first frame
        // DXGI hands out right after `DuplicateOutput`, whose texture is
        // not the desktop yet (encoding it produced a pure black 745-byte
        // IDR as the Instance's first frame; 3W-1-d-3 finding, frame_info
        // logged above). Anything ahead of the pacing schedule is released
        // without encoding too. Skipped frames are never lost: DXGI
        // accumulates dirty regions into the next acquired frame.
        if frame_info.LastPresentTime == 0 {
            stats.pointer_only += 1;
            if encoder.is_none() && start.elapsed() > Duration::from_secs(1) && !warned_no_image {
                // A completely static desktop produces no presents; the
                // first real frame (and so the Instance's first IDR) waits
                // for the next desktop update.
                eprintln!(
                    "[sardp-win] no desktop image update yet after {:?}",
                    start.elapsed()
                );
                warned_no_image = true;
            }
            continue;
        }
        if let Some(due) = next_due
            && now + Duration::from_millis(1) < due
        {
            stats.paced_out += 1;
            continue;
        }
        next_due = Some(match next_due {
            // Keep the schedule phase-locked to the refresh rather than to
            // our own (jittery) wake-ups, unless we've fallen well behind.
            Some(due) if now < due + min_interval => due + min_interval,
            _ => now + min_interval,
        });
        stats.encoded += 1;
        if stats.encoded.is_multiple_of(600) {
            eprintln!(
                "[sardp-win] capture: acquired={} encoded={} pointer_only={} paced_out={} dropped_by_consumer={}",
                stats.acquired,
                stats.encoded,
                stats.pointer_only,
                stats.paced_out,
                ctx.frames.dropped()
            );
        }
        let resource = resource.expect("AcquireNextFrame succeeded without a resource");
        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|err| e("frame texture cast", err))?;

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
            ctx.report_ready(SourceInfo {
                width: desc.Width,
                height: desc.Height,
                fps,
                origin_x: origin.0,
                origin_y: origin.1,
            });
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
            // Source-side drop under backpressure (DR-007) lives in the
            // shared FrameSender; a closed channel means the consumer is
            // gone and this worker is done.
            match ctx.frames.send(frame) {
                SendOutcome::Sent | SendOutcome::Dropped => {}
                SendOutcome::Closed => return Ok(()),
            }
        }
    }

    if let Some(mut enc) = encoder {
        let _ = enc.finish();
    }
    Ok(())
}

/// Capture-loop counters, logged every 600 encoded frames.
#[derive(Default)]
struct CaptureStats {
    acquired: u64,
    encoded: u64,
    pointer_only: u64,
    paced_out: u64,
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
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
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

        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        }];
        let blt = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams)
        };
        // The struct takes a raw COM pointer; release our AddRef'd clone
        // (this used to leak one reference per frame).
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        blt?;
        // Submit the copy+blit to the GPU now. The encoder MFT reads the
        // NV12 texture from its own queue; without this the first frame
        // it saw was the texture's zero-initialised contents (a pure black
        // 745-byte IDR at 2560x1440, 3W-1-d-3 finding), the blit only
        // landing on the GPU later.
        unsafe { context.Flush() };
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
    /// fallback source. The logic is `sardp::h264`'s (shared with macOS).
    parameter_sets: ParameterSetCache,
    /// `capture_ts` of inputs not yet matched to an output (1:1, no B-frames).
    pending_capture_ts: VecDeque<u64>,
    /// How long `encode_frame` waits for the current input's output before
    /// returning without it (one capture interval, at least 20ms).
    output_wait: Duration,
    /// `METransformNeedInput` events consumed while waiting for output,
    /// still to be spent on inputs.
    need_input_credits: u32,
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
        // GOP 1 = every frame an IDR. Otherwise a long GOP: IDRs are meant
        // to come from `request_idr` (generation open, spec 2.10) since
        // QUIC streams are lossless, so the periodic one is only a safety
        // net; 60s keeps it from mattering for bitrate.
        let gop_size = if config.all_idr { 1 } else { fps.max(1) * 60 };
        if let Some(api) = &codec_api {
            let gop = VARIANT::from(gop_size);
            if let Err(err) = unsafe { api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop) } {
                eprintln!("[sardp-win] CODECAPI_AVEncMPVGOPSize={gop_size} rejected: {err}");
            }
        } else {
            eprintln!(
                "[sardp-win] encoder MFT has no ICodecAPI; GOP size left at the driver default"
            );
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
            parameter_sets: ParameterSetCache::new(),
            pending_capture_ts: VecDeque::new(),
            output_wait: Duration::from_micros(1_000_000 / u64::from(fps.max(1)))
                .max(Duration::from_millis(20)),
            need_input_credits: 0,
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

        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)? };
        let sample: IMFSample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(duration)?;
        }

        let mut outputs = Vec::new();
        loop {
            // A NeedInput seen (and banked) while waiting for a previous
            // output is as good as one arriving now.
            let event_type = if self.need_input_credits > 0 {
                self.need_input_credits -= 1;
                METransformNeedInput.0 as u32
            } else {
                let event = self.wait_for_event(Duration::from_secs(5))?;
                unsafe { event.GetType()? }
            };
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
        // Wait (bounded) for this input's own output. Collecting only what
        // is *already* ready handed frame N's output over during frame
        // N+1's call, i.e. one capture interval of avoidable latency on
        // every frame (Stage 3 re-measurement: encode averaged 28ms with
        // ~17ms of that being the wait for the next capture). The bound
        // keeps an encoder that pipelines deeper than one frame from
        // stalling the capture loop: whatever isn't ready by then is
        // collected on the next call as before.
        let deadline = Instant::now() + self.output_wait;
        loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let event_type = unsafe { event.GetType()? };
                    if event_type == METransformHaveOutput.0 as u32 {
                        if let Some(frame) = self.take_one_output(clock)? {
                            outputs.push(frame);
                        }
                    } else if event_type == METransformNeedInput.0 as u32 {
                        // Don't lose the MFT's permission for the next
                        // input: the first version of this wait consumed
                        // it here and the next call then waited 5s for a
                        // NeedInput that had already been delivered.
                        self.need_input_credits += 1;
                    }
                }
                Err(_) => {
                    if self.output_count >= self.input_count || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
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
        let capture_ts = self
            .pending_capture_ts
            .pop_front()
            .unwrap_or(encode_done_ts);

        let annex_b = sample_bytes(&sample)?;
        // MFSampleExtension_CleanPoint proved unreliable on the validated
        // MFT (set on the first IDR only), so the NAL units decide.
        let clean_point =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let is_idr = h264::is_idr_access_unit(&annex_b, clean_point);
        if !self.parameter_sets.observe(&annex_b)
            && let Some(header) = self.read_sequence_header()
        {
            self.parameter_sets.set_fallback(header);
        }
        let annex_b = if is_idr {
            self.parameter_sets.complete_idr(annex_b)
        } else {
            annex_b
        };
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
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?
        };
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
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
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
