//! 3W-1-b: DXGI Desktop Duplicationのテクスチャ(GPU上)を、CPUへ読み戻さずに
//! Media Foundation Transform(MFT)経由のハードウェアH.264エンコーダへ渡し、
//! mp4ファイルへ書き出す。
//!
//! SARDP本体(sardp-server/sardp-client)とはまだ接続しない、3W-1-aと同じ位置づけの
//! 独立したサンプルバイナリ。3W-1-aの`release_frame_on_drop`(DXGIフレーム解放のRAII、
//! KNOWN_ISSUES #29の`DropGuard`)と同じ「スコープを抜けたら必ず解放する」考え方を
//! エンコーダ側リソース(IMFTransformのドレイン、IMFSinkWriterのFinalize)にも適用している。
//! ただしこちら側は`finished`フラグで二重呼び出しを防ぐ必要があり`DropGuard`には
//! 収まらないため、手書きの`impl Drop`のままにしている(KNOWN_ISSUES #29参照)。
//!
//! 実装メモ: 当初IMFSinkWriterの自動ハードウェア変換挿入(MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS)
//! を試したが、内部的には正しくNVIDIAのD3D11対応非同期MFTを選択しGetTransformForStreamで
//! 確認できるにもかかわらず、WriteSampleがE_INVALIDARGで失敗し続けた(原因は特定できず)。
//! ロードマップの「Media Foundation Transform(MFT)へ直接渡す経路を組む」という記述どおり、
//! IMFTransformを自前でイベント駆動(METransformNeedInput/METransformHaveOutput)で
//! 直接操作する構成に変更した。IMFSinkWriterはH.264ストリームをmp4へ詰めるだけの
//! 素通しmuxerとして使う(入力タイプ=出力タイプ=H264なので追加の変換は挿入されない)。

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
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
    IMFActivate, IMFDXGIDeviceManager, IMFMediaEvent, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFSinkWriter, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{GUID, HSTRING, Interface, PWSTR};

use dxgi_capture_poc::capture::{
    create_d3d11_device, create_output_duplication, primary_display_refresh_interval,
    read_dirty_rects, read_move_rect_count, release_frame_on_drop,
};

const ACQUIRE_TIMEOUT_MS: u32 = 500;
/// キャプチャセッションの目標時間(3W-1-aより少し長め)。実際に取得するフレーム数の
/// 上限は、この時間をディスプレイのリフレッシュ間隔で割って求める(MAX_FRAMES算出)。
const TARGET_SESSION_DURATION: Duration = Duration::from_secs(12);
const BITRATE_BPS: u32 = 8_000_000;

fn main() -> windows::core::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };

    // run()の中でD3D11/MFのCOMオブジェクトを全て生成・破棄してからMFShutdownを呼ぶ。
    // Rustのスコープ規則により、run()がreturnする時点でその内部で確保した
    // IMFTransform/IMFSinkWriter/IMFDXGIDeviceManager等は全てDropされ済みになる。
    let result = run();

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run() -> windows::core::Result<()> {
    let out_dir = output_dir();
    fs::create_dir_all(&out_dir).expect("failed to create output directory");
    let mp4_path = out_dir.join("capture.mp4");
    let log_path = out_dir.join("encode_log.txt");
    let mut log = BufWriter::new(File::create(&log_path).expect("failed to create log file"));

    // 以前はキャプチャ間隔を固定200ms(5fps)に絞っていたが、DXGI Desktop Duplicationは
    // 変化があった時だけAcquireNextFrameが返るため、上限を外しても無変化時の負荷は
    // 増えない。実ディスプレイのリフレッシュレートまで許容するようにする。
    let min_frame_interval = primary_display_refresh_interval();
    let nominal_fps = (1.0 / min_frame_interval.as_secs_f64()).round().max(1.0) as u32;
    let max_frames =
        (TARGET_SESSION_DURATION.as_secs_f64() / min_frame_interval.as_secs_f64()).ceil() as u32;

    println!("[mf-h264-encode] output: {}", mp4_path.display());
    println!("[mf-h264-encode] log: {}", log_path.display());
    println!(
        "[mf-h264-encode] min_frame_interval={:.2}ms nominal_fps={nominal_fps} max_frames={max_frames}",
        min_frame_interval.as_secs_f64() * 1000.0
    );
    writeln!(log, "# 3W-1-b Media Foundation H.264 hardware encode log").ok();
    writeln!(log, "# started_at={:?}", SystemTime::now()).ok();

    let (device, context) =
        create_d3d11_device(D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;

    // MFの内部ワーカースレッドが同じD3D11デバイスへアクセスするため、保護を有効化する
    // (Microsoft文書がD3D11デバイスをMFに渡す際の前提条件として明記している)。
    let multithread: ID3D11Multithread = device.cast()?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let device_manager = create_device_manager(&device)?;
    let duplication = create_output_duplication(&device)?;

    let mut converter: Option<VideoConverter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut encoded_frames = 0u32;
    let mut timeouts = 0u32;
    let start = Instant::now();

    while encoded_frames < max_frames {
        let frame_start = Instant::now();
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let acquire_result = unsafe {
            duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut frame_info, &mut resource)
        };

        match acquire_result {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                timeouts += 1;
                println!(
                    "[mf-h264-encode] timeout #{timeouts} (no desktop change within {ACQUIRE_TIMEOUT_MS}ms)"
                );
                continue;
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                eprintln!("[mf-h264-encode] DXGI_ERROR_ACCESS_LOST, stopping capture: {e}");
                break;
            }
            Err(e) => return Err(e),
        }

        // 3W-1-aと同じ`release_frame_on_drop`: 以降どの経路で抜けてもReleaseFrameを保証する。
        let frame_guard = release_frame_on_drop(&duplication);

        let resource = resource.expect("AcquireNextFrame succeeded without a resource");
        let texture: ID3D11Texture2D = resource.cast()?;

        let dirty_rects = read_dirty_rects(&duplication, &frame_info)?;
        let move_rect_count = read_move_rect_count(&duplication, &frame_info)?;

        if encoder.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            println!(
                "[mf-h264-encode] capture size: {}x{} (first frame determines encoder output size)",
                desc.Width, desc.Height
            );
            converter = Some(VideoConverter::new(
                &device,
                &context,
                desc.Width,
                desc.Height,
                nominal_fps,
            )?);
            encoder = Some(Encoder::new(
                &device_manager,
                &mp4_path,
                desc.Width,
                desc.Height,
                nominal_fps,
            )?);
        }

        // DXGI所有のフレームテクスチャを自前のBGRAテクスチャへコピーし
        // (このコピーが終わればReleaseFrameしてよい)、GPU上でNV12へ変換する。
        let nv12_texture = converter
            .as_ref()
            .expect("converter initialized above")
            .convert(&context, &texture)?;

        encoded_frames += 1;
        let elapsed_since_start = start.elapsed();
        log_encode_info(
            &mut log,
            encoded_frames,
            &frame_info,
            &dirty_rects,
            move_rect_count,
            elapsed_since_start,
        );
        println!(
            "[mf-h264-encode] frame {encoded_frames}/{max_frames}: dirty_rects={} move_rects={} elapsed={:.3}s",
            dirty_rects.len(),
            move_rect_count,
            elapsed_since_start.as_secs_f64(),
        );

        encoder
            .as_mut()
            .expect("encoder initialized above")
            .encode_frame(nv12_texture, elapsed_since_start)?;

        // 次のAcquireNextFrameより前に明示的に解放する。途中で`?`により抜けた場合は
        // DropGuardのdropが代わりに解放する。
        drop(frame_guard);

        let elapsed = frame_start.elapsed();
        if elapsed < min_frame_interval {
            std::thread::sleep(min_frame_interval - elapsed);
        }
    }

    if let Some(mut encoder) = encoder {
        encoder.finish()?;
    } else {
        eprintln!("[mf-h264-encode] no frames captured, nothing encoded");
    }
    log.flush().ok();
    println!(
        "[mf-h264-encode] done: {encoded_frames} frames encoded, {timeouts} timeouts, output={}",
        mp4_path.display()
    );
    Ok(())
}

fn output_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    exe_dir
        .join("..")
        .join("..")
        .join("captures")
        .join(format!("session_{timestamp}_h264"))
}

fn create_device_manager(device: &ID3D11Device) -> windows::core::Result<IMFDXGIDeviceManager> {
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
    let manager = manager.expect("device manager");
    unsafe { manager.ResetDevice(device, reset_token)? };
    Ok(manager)
}

/// MFSetAttributeSize/MFSetAttributeRatio相当。mfapi.hではヘッダオンリーの
/// インラインヘルパーとして定義されており(DLLエクスポートではない)win32metadataに
/// 現れないため、同じビットパッキング(上位32bit+下位32bit)を手で再実装する。
fn set_attribute_u64_pair(
    attrs: &IMFMediaType,
    key: &GUID,
    high: u32,
    low: u32,
) -> windows::core::Result<()> {
    let packed = ((high as u64) << 32) | (low as u64);
    unsafe { attrs.SetUINT64(key, packed) }
}

fn duration_to_100ns(d: Duration) -> i64 {
    (d.as_nanos() / 100) as i64
}

/// AcquireNextFrameが返すDXGI所有のBGRAテクスチャを、GPU上でNV12
/// (この環境のH.264ハードウェアエンコーダMFTが要求する入力フォーマット。
/// mf_probeで調査した結果、BGRAは受け付けないことが判明した)へ変換する。
/// ID3D11VideoProcessorを使うためCPUへの読み戻しは発生しない。
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

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
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
            Anonymous:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
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
            Anonymous:
                windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
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

    /// DXGIフレームを自前のBGRAテクスチャへコピーしてからNV12へ変換し、
    /// 変換先テクスチャへの参照を返す。
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
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                &[stream],
            )?;
        }
        Ok(&self.nv12)
    }
}

/// ハードウェアH.264エンコーダMFT(この環境ではNVIDIA H.264 Encoder MFT、非同期)を
/// 直接ProcessInput/ProcessOutputで駆動し、圧縮済みH.264サンプルを
/// [`Muxer`]経由でmp4へ書き出す。
///
/// [`Muxer`]がFinalize()を呼ばないとmp4のmoovアトムが書かれず再生不能になるのと同様、
/// このEncoderもMFT_MESSAGE_COMMAND_DRAINを送って未出力ぶんを回収しきらないと
/// 末尾のフレームが失われる。`release_frame_on_drop`と同じ「スコープを抜けたら
/// 必ず解放する」考え方でDropにフォールバックのfinish()呼び出しを持たせている
/// (通常経路では明示的に呼ぶ)。ただし`finished`フラグでの二重呼び出し防止が
/// `Drop::drop`自身のメソッドと状態を共有するため、`DropGuard`(KNOWN_ISSUES #29)
/// には置き換えず手書きの`impl Drop`のままにしている。
struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    muxer: Option<Muxer>,
    mp4_path: PathBuf,
    fps: u32,
    last_pts_100ns: Option<i64>,
    input_count: u32,
    output_count: u32,
    finished: bool,
}

impl Encoder {
    fn new(
        device_manager: &IMFDXGIDeviceManager,
        mp4_path: &Path,
        width: u32,
        height: u32,
        fps: u32,
    ) -> windows::core::Result<Self> {
        let activate = find_hardware_h264_encoder()?;
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };

        // 非同期MFT(このNVIDIAのMFTがそう)は、これを立てるまでSetOutputType等の
        // 呼び出し自体をMF_E_TRANSFORM_ASYNC_LOCKEDで拒否する。
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

        let output_type = unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE_BPS)?;
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

        let stream_info = unsafe { transform.GetOutputStreamInfo(0)? };
        eprintln!(
            "[mf-h264-encode] output stream info: dwFlags=0x{:08X} cbSize={} (PROVIDES_SAMPLES={}, CAN_PROVIDE_SAMPLES={})",
            stream_info.dwFlags,
            stream_info.cbSize,
            stream_info.dwFlags & 0x100 != 0,
            stream_info.dwFlags & 0x200 != 0,
        );

        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)? };
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)? };

        let events: IMFMediaEventGenerator = transform.cast()?;

        Ok(Self {
            transform,
            events,
            muxer: None,
            mp4_path: mp4_path.to_path_buf(),
            fps,
            last_pts_100ns: None,
            input_count: 0,
            output_count: 0,
            finished: false,
        })
    }

    fn encode_frame(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: Duration,
    ) -> windows::core::Result<()> {
        let pts = duration_to_100ns(timestamp);
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

        // METransformNeedInputが来るまで待ち(その間にHaveOutputが来たら先に汲み出す)、
        // 来たらProcessInputする。
        loop {
            let event = self.wait_for_event(Duration::from_secs(5))?;
            let event_type = unsafe { event.GetType()? };
            if event_type == METransformNeedInput.0 as u32 {
                unsafe { self.transform.ProcessInput(0, &sample, 0)? };
                self.input_count += 1;
                break;
            } else if event_type == METransformHaveOutput.0 as u32 {
                self.drain_one_output()?;
            }
        }

        // 直後に追加でHaveOutputが溜まっていれば非ブロッキングで汲んでおく。
        self.drain_available_nonblocking()?;
        Ok(())
    }

    /// ブロッキング版のGetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))は、この環境
    /// (非同期NVIDIA MFT)では実際に返ってこない(ProcessInput/finish()のドレインの両方で
    /// 確認済み)ため、MF_EVENT_FLAG_NO_WAITでポーリングしてタイムアウトを持たせる。
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
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn drain_one_output(&mut self) -> windows::core::Result<()> {
        let mut output_buffer = MFT_OUTPUT_DATA_BUFFER::default();
        output_buffer.dwStreamID = 0;
        let mut status = 0u32;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output_buffer), &mut status)
        };
        match result {
            Ok(()) => {
                if let Some(sample) = output_buffer.pSample.take() {
                    self.output_count += 1;
                    self.forward_sample(&sample)?;
                } else {
                    eprintln!(
                        "[mf-h264-encode] ProcessOutput ok but pSample=None (status=0x{status:08X})"
                    );
                }
                Ok(())
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn drain_available_nonblocking(&mut self) -> windows::core::Result<()> {
        loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let event_type = unsafe { event.GetType()? };
                    if event_type == METransformHaveOutput.0 as u32 {
                        self.drain_one_output()?;
                    }
                }
                Err(_) => break, // MF_E_NO_EVENTS_AVAILABLE: 今は何も来ていない
            }
        }
        Ok(())
    }

    /// エンコード済みサンプルをmuxerへ渡す。初回はエンコーダが実際に確定した
    /// 出力タイプ(SPS/PPS等のシーケンスヘッダを含む)を使ってMuxerを遅延生成する。
    fn forward_sample(&mut self, sample: &IMFSample) -> windows::core::Result<()> {
        if self.muxer.is_none() {
            let negotiated_type = unsafe { self.transform.GetOutputCurrentType(0)? };
            self.muxer = Some(Muxer::new(&self.mp4_path, &negotiated_type)?);
        }
        self.muxer
            .as_mut()
            .expect("muxer initialized above")
            .write_encoded_sample(sample)
    }

    fn finish(&mut self) -> windows::core::Result<()> {
        if self.finished {
            return Ok(());
        }
        eprintln!(
            "[mf-h264-encode] draining; {}/{} samples produced so far",
            self.output_count, self.input_count
        );
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?
        };

        // 入力1フレームにつき出力1サンプル(Bフレーム無し)が来るはずなので、
        // 提出した入力数ぶんの出力が揃った時点で完了とみなす。これが主判定。
        // METransformNeedInputイベントは、ドレイン後にこのMFTから来るとは限らない
        // (実測で確認済み)ため、イベント待ちはあくまでフォールバックの安全弁とする。
        //
        // wait_for_event自体は個々にタイムアウトするが、それだけではHaveOutputイベントが
        // 進捗(output_countの増加)を伴わずに来続けた場合にループ全体が終わらない
        // 可能性が残る(レビュー指摘)。ドレイン全体にも締め切りを設け、超過したら
        // その時点までに集まった分で諦めてエラーを返す。
        let drain_deadline = Instant::now() + Duration::from_secs(30);
        while self.output_count < self.input_count {
            if Instant::now() >= drain_deadline {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    format!(
                        "drain did not complete within 30s ({}/{} samples produced)",
                        self.output_count, self.input_count
                    ),
                ));
            }
            let event = self.wait_for_event(Duration::from_secs(10))?;
            let event_type = unsafe { event.GetType()? };
            if event_type == METransformHaveOutput.0 as u32 {
                self.drain_one_output()?;
            } else if event_type == METransformNeedInput.0 as u32 {
                break;
            }
        }
        eprintln!(
            "[mf-h264-encode] drain complete; {}/{} samples produced total",
            self.output_count, self.input_count
        );

        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .ok()
        };
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                .ok()
        };

        if let Some(muxer) = self.muxer.as_mut() {
            muxer.finalize()?;
        } else {
            eprintln!("[mf-h264-encode] no encoded samples were produced, mp4 not created");
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.finished {
            eprintln!("[mf-h264-encode] Encoder dropped without explicit finish(); draining now");
            if let Err(e) = self.finish() {
                eprintln!("[mf-h264-encode] finish() on drop failed: {e}");
            }
        }
    }
}

/// ハードウェア(HARDWARE enumフラグ)かつ同期呼び出し可能なH.264エンコーダMFTを
/// 列挙して先頭(通常GPUベンダー製が優先される)を返す。
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
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "no hardware H.264 encoder MFT found",
        ));
    }

    // MFTEnumExが返す配列は、配列自体のメモリ(CoTaskMemFreeで解放)と、各要素が保持する
    // IMFActivateへの強参照(個別にReleaseが必要)が別物。Option::take()で各スロットの
    // 所有権をRust側へ正しく取り出せば、使わない要素はそのままループを抜ける際にDropし
    // (windows-rsのCOMラッパーがDropでRelease()を呼ぶ)、使う最初の1要素だけを保持する。
    // 以前はslice[0].clone()で複製を取るだけだったため、配列内の全要素(1番目の元参照を
    // 含む)がリークしていた。
    let slice: &mut [Option<IMFActivate>] =
        unsafe { std::slice::from_raw_parts_mut(activates, count as usize) };
    let mut first: Option<IMFActivate> = None;
    for (i, slot) in slice.iter_mut().enumerate() {
        let owned = slot.take();
        if i == 0 {
            first = owned;
        }
        // i != 0の場合、ここでownedがスコープを抜けてReleaseされる。
    }
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
    let first = first.expect("first activate present");

    let mut name_buf = String::new();
    if let Ok(name) = unsafe {
        let mut ptr = PWSTR::null();
        let mut len = 0u32;
        first
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len)
            .map(|()| ptr.to_string().unwrap_or_default())
    } {
        name_buf = name;
    }
    println!("[mf-h264-encode] using hardware encoder MFT: {name_buf}");
    Ok(first)
}

/// エンコード済みH.264サンプルをmp4へ詰めるだけのmuxer。追加の変換は挿入されない
/// (入力タイプ=出力タイプ=H264、かつエンコーダが確定した実際の出力タイプ
/// [SPS/PPS等のシーケンスヘッダ込み]をそのまま使う)。
struct Muxer {
    writer: IMFSinkWriter,
    stream_index: u32,
    finalized: bool,
}

impl Muxer {
    fn new(path: &Path, negotiated_type: &IMFMediaType) -> windows::core::Result<Self> {
        let path_str = path.to_string_lossy().into_owned();
        let url = HSTRING::from(path_str);
        let writer: IMFSinkWriter = unsafe { MFCreateSinkWriterFromURL(&url, None, None)? };

        let stream_index = unsafe { writer.AddStream(negotiated_type)? };
        unsafe { writer.SetInputMediaType(stream_index, negotiated_type, None)? };
        unsafe { writer.BeginWriting()? };

        Ok(Self {
            writer,
            stream_index,
            finalized: false,
        })
    }

    fn write_encoded_sample(&mut self, sample: &IMFSample) -> windows::core::Result<()> {
        unsafe { self.writer.WriteSample(self.stream_index, sample) }
    }

    fn finalize(&mut self) -> windows::core::Result<()> {
        if !self.finalized {
            unsafe { self.writer.Finalize()? };
            self.finalized = true;
        }
        Ok(())
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        if !self.finalized {
            eprintln!("[mf-h264-encode] Muxer dropped without explicit finalize(); finalizing now");
            if let Err(e) = unsafe { self.writer.Finalize() } {
                eprintln!("[mf-h264-encode] Finalize on drop failed: {e}");
            }
            self.finalized = true;
        }
    }
}

fn log_encode_info(
    log: &mut impl Write,
    index: u32,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
    dirty_rects: &[RECT],
    move_rect_count: usize,
    elapsed: Duration,
) {
    writeln!(
        log,
        "frame={index} elapsed_s={:.3} last_present_qpc={} accumulated_frames={} pointer_visible={} total_metadata_buffer_size={} move_rects={} dirty_rects={}",
        elapsed.as_secs_f64(),
        frame_info.LastPresentTime,
        frame_info.AccumulatedFrames,
        frame_info.PointerPosition.Visible.as_bool(),
        frame_info.TotalMetadataBufferSize,
        move_rect_count,
        dirty_rects.len(),
    )
    .ok();
    for (i, r) in dirty_rects.iter().enumerate() {
        writeln!(
            log,
            "  dirty[{i}] left={} top={} right={} bottom={}",
            r.left, r.top, r.right, r.bottom
        )
        .ok();
    }
}
