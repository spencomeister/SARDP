//! DXGI Desktop Duplicationのデバイス作成・フレーム取得まわり。
//! 3W-1-a(BMPダンプ, `src/main.rs`)と3W-1-b(H.264エンコード,
//! `src/bin/mf_h264_encode.rs`)の両方から共有される。

use std::mem::size_of;
use std::time::Duration;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_FLAG,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};

/// AcquireNextFrame成功後、確実にReleaseFrameを対応させるRAIIガード。
/// Desktop Duplicationは未解放フレームを1つしか許さないため、
/// 以降の処理が`?`で早期returnしてもpanicでunwindしても解放漏れが起きないようにする。
pub struct FrameGuard<'a> {
    pub duplication: &'a IDXGIOutputDuplication,
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = unsafe { self.duplication.ReleaseFrame() } {
            eprintln!("[capture] ReleaseFrame failed: {e}");
        }
    }
}

/// D3D11デバイス+コンテキストを作成する。
/// `extra_flags`でMedia Foundation連携に必要な`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`等を追加できる。
pub fn create_d3d11_device(
    extra_flags: D3D11_CREATE_DEVICE_FLAG,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let adapter1: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(0) }?;
    let adapter: IDXGIAdapter = adapter1.cast()?;

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            extra_flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }

    Ok((device.expect("device"), context.expect("context")))
}

pub fn create_output_duplication(
    device: &ID3D11Device,
) -> windows::core::Result<IDXGIOutputDuplication> {
    let dxgi_device: IDXGIAdapter = unsafe {
        device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
            .GetAdapter()?
    };
    let output: IDXGIOutput = unsafe { dxgi_device.EnumOutputs(0) }?;
    let output1: IDXGIOutput1 = output.cast()?;
    unsafe { output1.DuplicateOutput(device) }
}

/// Desktop-coordinate rectangle of the output [`create_output_duplication`]
/// duplicates (output 0 of the device's adapter), i.e. where the captured
/// image sits in the virtual desktop -- what input coordinates relative
/// to the captured image must be offset by before injection.
pub fn duplicated_output_desktop_rect(device: &ID3D11Device) -> windows::core::Result<RECT> {
    let adapter: IDXGIAdapter = unsafe {
        device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
            .GetAdapter()?
    };
    let output: IDXGIOutput = unsafe { adapter.EnumOutputs(0) }?;
    let desc = unsafe { output.GetDesc()? };
    Ok(desc.DesktopCoordinates)
}

pub fn read_dirty_rects(
    duplication: &IDXGIOutputDuplication,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
) -> windows::core::Result<Vec<RECT>> {
    if frame_info.TotalMetadataBufferSize == 0 {
        return Ok(Vec::new());
    }
    // Vec<RECT>として確保することでRECTのアラインメントを型システムに保証させる
    // (Vec<u8>をas *mut RECTでキャストするのはアロケータの実務上の挙動に依存したUB)。
    let capacity = (frame_info.TotalMetadataBufferSize as usize).div_ceil(size_of::<RECT>());
    let mut buf: Vec<RECT> = vec![RECT::default(); capacity];
    let mut needed = 0u32;
    unsafe {
        duplication.GetFrameDirtyRects(
            (buf.len() * size_of::<RECT>()) as u32,
            buf.as_mut_ptr(),
            &mut needed,
        )?;
    }
    let count = needed as usize / size_of::<RECT>();
    buf.truncate(count);
    Ok(buf)
}

/// プライマリディスプレイの現在のリフレッシュレートから、1フレームあたりの間隔を返す。
/// 取得できない場合は60Hz相当にフォールバックする。
///
/// 当初はキャプチャ間隔を固定200ms(5fps相当)に絞っていたが、DXGI Desktop Duplicationは
/// 変化があった時だけAcquireNextFrameが返るため、上限を外しても無変化時の負荷が増える
/// わけではない。むしろ実際に変化が速い場面(動画再生・スクロール等)で不要に
/// 間引いてしまっていたため、ディスプレイの実リフレッシュレートまで許容するように変更。
pub fn primary_display_refresh_interval() -> Duration {
    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode) }
        .as_bool();
    // dmDisplayFrequencyの0/1は「ハードウェア既定値・不明」を意味する(MSDN)ため、
    // その場合も60Hzにフォールバックする。
    let hz = if ok && devmode.dmDisplayFrequency > 1 {
        devmode.dmDisplayFrequency
    } else {
        60
    };
    Duration::from_secs_f64(1.0 / hz as f64)
}

pub fn read_move_rect_count(
    duplication: &IDXGIOutputDuplication,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
) -> windows::core::Result<usize> {
    if frame_info.TotalMetadataBufferSize == 0 {
        return Ok(0);
    }
    let capacity =
        (frame_info.TotalMetadataBufferSize as usize).div_ceil(size_of::<DXGI_OUTDUPL_MOVE_RECT>());
    let mut buf: Vec<DXGI_OUTDUPL_MOVE_RECT> = vec![DXGI_OUTDUPL_MOVE_RECT::default(); capacity];
    let mut needed = 0u32;
    unsafe {
        duplication.GetFrameMoveRects(
            (buf.len() * size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
            buf.as_mut_ptr(),
            &mut needed,
        )?;
    }
    Ok(needed as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>())
}
