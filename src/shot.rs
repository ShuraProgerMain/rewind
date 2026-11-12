//! Screenshots: one picture of what is being recorded, saved as PNG next to the videos.
//!
//! A screenshot opens its own short capture session instead of borrowing frames from the
//! recording pipeline, so it works the same whether the buffer runs or not, and a picture
//! that has not changed for a while (where the pipeline gets no new frames) is no problem.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{GENERIC_WRITE, HWND};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Resource, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat24bppBGR, IWICImagingFactory,
    WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::capture::{Capture, Source};
use crate::d3d::create_gpu;
use crate::engine::Config;

/// Takes a screenshot of the configured source (and area) and saves it. Blocks for a few
/// tens of milliseconds; call it on a worker thread.
pub fn take(cfg: &Config) -> Result<PathBuf> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let (pixels, width, height) = grab(cfg)?;
    let path = crate::engine::out_path(cfg, "Скриншот", "png")?;
    if let Err(e) = save_png(&path, &pixels, width, height) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// The picture as 24-bit BGR rows, top to bottom.
fn grab(cfg: &Config) -> Result<(Vec<u8>, u32, u32)> {
    let monitor = match cfg.source {
        Source::Monitor(h) => h,
        Source::Window(h) => unsafe { MonitorFromWindow(HWND(h as _), MONITOR_DEFAULTTONEAREST).0 as isize },
    };
    let gpu = create_gpu(monitor, None)?;
    let mut cap = Capture::new(&gpu, cfg.source, 60, cfg.cursor)?;
    // The first frame arrives right after the session starts (a minimised window sends none).
    let deadline = Instant::now() + Duration::from_millis(1500);
    let frame = loop {
        if let Some(frame) = cap.latest()? {
            break frame;
        }
        if cap.is_closed() || Instant::now() > deadline {
            bail!("no picture: the window is minimised or the display is off");
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let (fw, fh) = (frame.width, frame.height);
    let (x, y, w, h) = match cfg.crop {
        Some((x, y, w, h)) => {
            let x = x.min(fw.saturating_sub(1));
            let y = y.min(fh.saturating_sub(1));
            (x, y, w.min(fw - x), h.min(fh - y))
        }
        None => (0, 0, fw, fh),
    };
    if w == 0 || h == 0 {
        bail!("the picture is empty");
    }
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;
        let area = D3D11_BOX { left: x, top: y, front: 0, right: x + w, bottom: y + h, back: 1 };
        let dst: ID3D11Resource = staging.cast()?;
        let src: ID3D11Resource = frame.texture.cast()?;
        gpu.context.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, Some(&area));
        drop(frame);

        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        let mut out = Vec::with_capacity((w * h * 3) as usize);
        for row in 0..h as usize {
            let line = std::slice::from_raw_parts((map.pData as *const u8).add(row * map.RowPitch as usize), w as usize * 4);
            for px in line.chunks_exact(4) {
                out.extend_from_slice(&px[..3]); // BGRA -> BGR: captured alpha means nothing
            }
        }
        gpu.context.Unmap(&dst, 0);
        Ok((out, w, h))
    }
}

/// Writes 24-bit BGR rows as a PNG file (Windows' own image codecs).
fn save_png(path: &Path, bgr: &[u8], width: u32, height: u32) -> Result<()> {
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), GENERIC_WRITE.0)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(|| anyhow!("no image frame"))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        if format != GUID_WICPixelFormat24bppBGR {
            bail!("the PNG encoder does not take 24-bit pictures");
        }
        frame.WritePixels(height, width * 3, bgr)?;
        frame.Commit()?;
        encoder.Commit()?;
        stream.Commit(windows::Win32::System::Com::STGC_DEFAULT)?;
    }
    Ok(())
}
