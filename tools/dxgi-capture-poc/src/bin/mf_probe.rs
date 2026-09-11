//! 診断ツール: このマシンで利用可能なハードウェアH.264エンコーダMFTを列挙し、
//! D3D11対応の有無(MF_SA_D3D11_AWARE)・非同期かどうか(MF_TRANSFORM_ASYNC)・
//! D3D管理オブジェクトを教えた上での実際の対応入力フォーマットを表示する。
//!
//! 3W-1-bの実装時、IMFSinkWriterの自動ハードウェア変換選択がWriteSampleで
//! E_INVALIDARGを返す原因切り分けに使った。同種のMFT絡みの問題が出た際に
//! 再利用できるよう残してある(mf_h264_encode.rsとは独立したサンプルバイナリ)。

use windows::core::{Interface, GUID};
use windows::core::PWSTR;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaType, IMFTransform, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFStartup, MFShutdown, MFTEnumEx, MFMediaType_Video, MFVideoFormat_H264,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFSTARTUP_FULL,
};

fn set_u64_pair(t: &IMFMediaType, key: &GUID, hi: u32, lo: u32) -> windows::core::Result<()> {
    unsafe { t.SetUINT64(key, ((hi as u64) << 32) | (lo as u64)) }
}
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

fn main() -> windows::core::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };

    let result = run();

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run() -> windows::core::Result<()> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let adapter1: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(0)? };
    let adapter: IDXGIAdapter = adapter1.cast()?;
    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.expect("device");
    let mut reset_token = 0u32;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
    let manager = manager.expect("manager");
    unsafe { manager.ResetDevice(&device, reset_token)? };

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
    println!("[mf-probe] found {count} hardware H.264 encoder MFT(s)");

    let activates_slice: &[Option<IMFActivate>] =
        unsafe { std::slice::from_raw_parts(activates, count as usize) };

    for (i, activate) in activates_slice.iter().enumerate() {
        let Some(activate) = activate else { continue };
        let name = unsafe {
            let mut ptr = PWSTR::null();
            let mut len = 0u32;
            match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) {
                Ok(()) => ptr.to_string().unwrap_or_default(),
                Err(_) => "<unknown>".to_string(),
            }
        };
        println!("[mf-probe] [{i}] {name}");

        let transform: windows::core::Result<IMFTransform> = unsafe { activate.ActivateObject() };
        let transform = match transform {
            Ok(t) => t,
            Err(e) => {
                println!("[mf-probe]     ActivateObject failed: {e}");
                continue;
            }
        };

        // D3D11-awareかどうか(このMFTがDXGIサーフェスバッファを直接受け取れるか)を
        // MFT自身のIMFAttributesから確認する。
        let attrs = unsafe { transform.GetAttributes() };
        match attrs {
            Ok(attrs) => {
                let d3d11_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0);
                println!("[mf-probe]     MF_SA_D3D11_AWARE = {d3d11_aware}");
            }
            Err(e) => println!("[mf-probe]     GetAttributes failed: {e}"),
        }

        // 非同期MFT(NVIDIAのものがそう)はMF_TRANSFORM_ASYNC_UNLOCKを立てるまで
        // ProcessMessage/SetOutputType等の呼び出し自体を拒否する
        // (MF_E_TRANSFORM_ASYNC_LOCKED = 0xC00D6D77)。
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            if let Err(e) = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) } {
                println!("[mf-probe]     SetUINT32(ASYNC_UNLOCK) failed: {e}");
            }
        }

        // D3D11-awareなMFTは、device managerを教えるまで入力タイプの列挙自体を
        // 拒否する可能性があるため、先に送っておく。
        let raw_manager = Interface::as_raw(&manager) as usize;
        if let Err(e) = unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, raw_manager) } {
            println!("[mf-probe]     ProcessMessage(SET_D3D_MANAGER) failed: {e}");
        }

        // 出力タイプを先に設定しないとGetInputAvailableTypeが何も返さないMFTがある
        // (実際に3W-1-bで踏んだ)ため、H264出力タイプを設定してから入力側を列挙する。
        let out_type = unsafe { MFCreateMediaType() };
        match out_type {
            Ok(t) => {
                let setup = (|| -> windows::core::Result<()> {
                    unsafe {
                        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                        t.SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000)?;
                        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                        set_u64_pair(&t, &MF_MT_FRAME_SIZE, 2560, 1440)?;
                        set_u64_pair(&t, &MF_MT_FRAME_RATE, 5, 1)?;
                        transform.SetOutputType(0, &t, 0)?;
                    }
                    Ok(())
                })();
                if let Err(e) = setup {
                    println!("[mf-probe]     SetOutputType failed: {e}");
                }
            }
            Err(e) => println!("[mf-probe]     MFCreateMediaType failed: {e}"),
        }

        // 入力側でサポートしているサブタイプを列挙する。
        let mut idx = 0u32;
        loop {
            let t = unsafe { transform.GetInputAvailableType(0, idx) };
            match t {
                Ok(media_type) => {
                    let subtype = unsafe {
                        media_type.GetGUID(&windows::Win32::Media::MediaFoundation::MF_MT_SUBTYPE)
                    };
                    match subtype {
                        Ok(g) => println!("[mf-probe]     input[{idx}] subtype = {g:?}"),
                        Err(e) => println!("[mf-probe]     input[{idx}] GetGUID failed: {e}"),
                    }
                    idx += 1;
                }
                Err(_) => break, // MF_E_NO_MORE_TYPES
            }
        }

        unsafe { activate.ShutdownObject().ok() };
    }

    unsafe {
        for a in activates_slice.iter().flatten() {
            let _ = a;
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
    }

    Ok(())
}
