//! 3W-1-a: DXGI Desktop Duplication 単体疎通確認。
//!
//! SARDP本体(sardp-server/sardp-client)とは接続しない、独立したサンプルバイナリ。
//! デスクトップを毎秒数フレーム取得し、連番BMPとしてディスクへ保存しつつ、
//! DXGI_OUTDUPL_FRAME_INFOのダーティリージョン情報をログ出力する。

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};

/// 1フレーム取得を待つ最大時間(ms)。これを超えると「変化なし」としてリトライする。
const ACQUIRE_TIMEOUT_MS: u32 = 500;
/// 取得するフレーム数の上限(PoCなのでキャプチャセッションの長さを制限する)。
const MAX_FRAMES: u32 = 30;
/// フレーム取得後、次のAcquireNextFrameまでの最小間隔。「毎秒数フレーム」に収める。
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(200);

fn main() -> windows::core::Result<()> {
    let out_dir = output_dir();
    fs::create_dir_all(&out_dir).expect("failed to create output directory");
    let log_path = out_dir.join("dirty_regions.log");
    let mut log = BufWriter::new(File::create(&log_path).expect("failed to create log file"));

    println!("[dxgi-capture-poc] output directory: {}", out_dir.display());
    println!("[dxgi-capture-poc] log file: {}", log_path.display());
    writeln!(log, "# 3W-1-a DXGI Desktop Duplication capture log").ok();
    writeln!(log, "# started_at={:?}", SystemTime::now()).ok();

    let (device, context) = create_d3d11_device()?;
    let duplication = create_output_duplication(&device)?;

    let mut saved_frames = 0u32;
    let mut timeouts = 0u32;
    let start = Instant::now();

    while saved_frames < MAX_FRAMES {
        let frame_start = Instant::now();
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let acquire_result =
            unsafe { duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut frame_info, &mut resource) };

        match acquire_result {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                timeouts += 1;
                println!(
                    "[dxgi-capture-poc] timeout #{timeouts} (no desktop change within {ACQUIRE_TIMEOUT_MS}ms)"
                );
                continue;
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                eprintln!("[dxgi-capture-poc] DXGI_ERROR_ACCESS_LOST, stopping capture: {e}");
                break;
            }
            Err(e) => return Err(e),
        }

        let resource = resource.expect("AcquireNextFrame succeeded without a resource");
        let texture: ID3D11Texture2D = resource.cast()?;

        let dirty_rects = read_dirty_rects(&duplication, &frame_info)?;
        let move_rect_count = read_move_rect_count(&duplication, &frame_info)?;

        saved_frames += 1;
        let elapsed_since_start = start.elapsed();
        log_frame_info(&mut log, saved_frames, &frame_info, &dirty_rects, move_rect_count, elapsed_since_start);
        println!(
            "[dxgi-capture-poc] frame {saved_frames}/{MAX_FRAMES}: dirty_rects={} move_rects={} accumulated_frames={} last_present_qpc={} elapsed={:.3}s",
            dirty_rects.len(),
            move_rect_count,
            frame_info.AccumulatedFrames,
            frame_info.LastPresentTime,
            elapsed_since_start.as_secs_f64(),
        );

        let bmp_path = out_dir.join(format!("frame_{saved_frames:05}.bmp"));
        save_texture_as_bmp(&device, &context, &texture, &bmp_path)?;
        println!("[dxgi-capture-poc]   saved: {}", bmp_path.display());

        unsafe { duplication.ReleaseFrame()? };

        let elapsed = frame_start.elapsed();
        if elapsed < MIN_FRAME_INTERVAL {
            std::thread::sleep(MIN_FRAME_INTERVAL - elapsed);
        }
    }

    log.flush().ok();
    println!(
        "[dxgi-capture-poc] done: {saved_frames} frames saved, {timeouts} timeouts, output={}",
        out_dir.display()
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
    // target/{debug,release}配下から見て安定した場所に captures/ を作る。
    exe_dir
        .join("..")
        .join("..")
        .join("captures")
        .join(format!("session_{timestamp}"))
}

fn create_d3d11_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
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
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }

    Ok((device.expect("device"), context.expect("context")))
}

fn create_output_duplication(device: &ID3D11Device) -> windows::core::Result<IDXGIOutputDuplication> {
    let dxgi_device: IDXGIAdapter = unsafe {
        device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
            .GetAdapter()?
    };
    let output: IDXGIOutput = unsafe { dxgi_device.EnumOutputs(0) }?;
    let output1: IDXGIOutput1 = output.cast()?;
    unsafe { output1.DuplicateOutput(device) }
}

fn read_dirty_rects(
    duplication: &IDXGIOutputDuplication,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
) -> windows::core::Result<Vec<RECT>> {
    if frame_info.TotalMetadataBufferSize == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; frame_info.TotalMetadataBufferSize as usize];
    let mut needed = 0u32;
    unsafe {
        duplication.GetFrameDirtyRects(buf.len() as u32, buf.as_mut_ptr() as *mut RECT, &mut needed)?;
    }
    let count = needed as usize / size_of::<RECT>();
    let rects: &[RECT] =
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const RECT, count) };
    Ok(rects.to_vec())
}

fn read_move_rect_count(
    duplication: &IDXGIOutputDuplication,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
) -> windows::core::Result<usize> {
    use windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_MOVE_RECT;
    if frame_info.TotalMetadataBufferSize == 0 {
        return Ok(0);
    }
    let mut buf = vec![0u8; frame_info.TotalMetadataBufferSize as usize];
    let mut needed = 0u32;
    unsafe {
        duplication.GetFrameMoveRects(
            buf.len() as u32,
            buf.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT,
            &mut needed,
        )?;
    }
    Ok(needed as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>())
}

fn log_frame_info(
    log: &mut impl Write,
    index: u32,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
    dirty_rects: &[RECT],
    move_rect_count: usize,
    elapsed: Duration,
) {
    writeln!(
        log,
        "frame={index} elapsed_s={:.3} last_present_qpc={} last_mouse_update_qpc={} accumulated_frames={} rects_coalesced={} protected_content_masked_out={} pointer_visible={} pointer_x={} pointer_y={} total_metadata_buffer_size={} move_rects={} dirty_rects={}",
        elapsed.as_secs_f64(),
        frame_info.LastPresentTime,
        frame_info.LastMouseUpdateTime,
        frame_info.AccumulatedFrames,
        frame_info.RectsCoalesced.as_bool(),
        frame_info.ProtectedContentMaskedOut.as_bool(),
        frame_info.PointerPosition.Visible.as_bool(),
        frame_info.PointerPosition.Position.x,
        frame_info.PointerPosition.Position.y,
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

fn save_texture_as_bmp(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    path: &Path,
) -> windows::core::Result<()> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    assert_eq!(
        desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM,
        "unexpected DXGI capture format"
    );

    let mut staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;

    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.expect("staging texture");

    let src: ID3D11Resource = texture.cast()?;
    let dst: ID3D11Resource = staging.cast()?;
    unsafe { context.CopyResource(&dst, &src) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

    let width = desc.Width;
    let height = desc.Height;
    let row_pitch = mapped.RowPitch as usize;
    let data = unsafe {
        std::slice::from_raw_parts(mapped.pData as *const u8, row_pitch * height as usize)
    };

    let result = write_bmp(path, data, row_pitch, width, height);

    unsafe { context.Unmap(&dst, 0) };

    result.expect("failed to write BMP");
    Ok(())
}

/// BGRA8ソースから24bit BMP(アルファは捨てる)を書き出す。
/// BMPは下から上へ格納するため、行を逆順に書く。
fn write_bmp(
    path: &Path,
    bgra_data: &[u8],
    row_pitch: usize,
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    let bytes_per_pixel_out = 3usize;
    let row_size_out = (width as usize * bytes_per_pixel_out + 3) & !3; // 4バイト境界にパディング
    let pixel_data_size = row_size_out * height as usize;
    let file_header_size = 14u32;
    let info_header_size = 40u32;
    let pixel_data_offset = file_header_size + info_header_size;
    let file_size = pixel_data_offset + pixel_data_size as u32;

    let mut f = BufWriter::new(File::create(path)?);

    // BITMAPFILEHEADER
    f.write_all(b"BM")?;
    f.write_all(&file_size.to_le_bytes())?;
    f.write_all(&0u16.to_le_bytes())?;
    f.write_all(&0u16.to_le_bytes())?;
    f.write_all(&pixel_data_offset.to_le_bytes())?;

    // BITMAPINFOHEADER
    f.write_all(&info_header_size.to_le_bytes())?;
    f.write_all(&(width as i32).to_le_bytes())?;
    f.write_all(&(height as i32).to_le_bytes())?; // 正 = ボトムアップ
    f.write_all(&1u16.to_le_bytes())?; // planes
    f.write_all(&24u16.to_le_bytes())?; // bpp
    f.write_all(&0u32.to_le_bytes())?; // compression = BI_RGB
    f.write_all(&(pixel_data_size as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?; // x pixels/meter (~72dpi)
    f.write_all(&2835i32.to_le_bytes())?; // y pixels/meter
    f.write_all(&0u32.to_le_bytes())?; // colors used
    f.write_all(&0u32.to_le_bytes())?; // important colors

    let pad = vec![0u8; row_size_out - width as usize * bytes_per_pixel_out];
    // ボトムアップなので最終行から書く
    for y in (0..height as usize).rev() {
        let row_start = y * row_pitch;
        let row = &bgra_data[row_start..row_start + width as usize * 4];
        for px in row.chunks_exact(4) {
            // px = [B, G, R, A] -> BMPも [B, G, R] の順
            f.write_all(&px[0..3])?;
        }
        if !pad.is_empty() {
            f.write_all(&pad)?;
        }
    }

    Ok(())
}
