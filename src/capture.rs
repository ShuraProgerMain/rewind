//! Windows Graphics Capture: frames stay in video memory as D3D11 textures.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use windows::core::{factory, Interface};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d::Gpu;

/// What to record. Handles are kept as integers so the value can cross threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Monitor(isize),
    Window(isize),
}

pub struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: IDirect3DDevice,
    pool_size: SizeInt32,
    closed: Arc<AtomicBool>,
}

/// One captured frame; the texture is valid until this is dropped.
pub struct Frame {
    frame: Direct3D11CaptureFrame,
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

impl Drop for Frame {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

impl Capture {
    pub fn new(gpu: &Gpu, source: Source, fps: u32, cursor: bool) -> Result<Self> {
        unsafe {
            let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = match source {
                Source::Monitor(h) => interop.CreateForMonitor(HMONITOR(h as _))?,
                Source::Window(h) => interop.CreateForWindow(HWND(h as _))?,
            };

            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;

            let pool_size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, FORMAT, 2, pool_size)?;
            let session = pool.CreateCaptureSession(&item)?;

            // Optional niceties: each exists only on newer Windows builds, so failures are ignored.
            let _ = session.SetIsCursorCaptureEnabled(cursor);
            let _ = session.SetIsBorderRequired(false);
            // Do not let the compositor hand us more frames than we encode.
            let interval = 10_000_000i64 / fps as i64 - 20_000;
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: interval });

            let closed = Arc::new(AtomicBool::new(false));
            let flag = closed.clone();
            item.Closed(&TypedEventHandler::new(move |_, _| {
                flag.store(true, Ordering::Relaxed);
                Ok(())
            }))?;

            session.StartCapture()?;
            Ok(Self { _item: item, pool, session, device, pool_size, closed })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.pool_size.Width.max(0) as u32, self.pool_size.Height.max(0) as u32)
    }

    /// True once the captured window or monitor is gone.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Newest frame since the last call, if the picture changed. Older queued frames are discarded.
    pub fn latest(&mut self) -> Result<Option<Frame>> {
        let mut newest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(old) = newest.replace(frame) {
                let _ = old.Close();
            }
        }
        let Some(frame) = newest else { return Ok(None) };

        let content = frame.ContentSize()?;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let out = Frame {
            frame,
            texture,
            width: content.Width.min(self.pool_size.Width).max(0) as u32,
            height: content.Height.min(self.pool_size.Height).max(0) as u32,
        };

        // A resized window needs bigger (or smaller) buffers for the following frames.
        if content.Width > 0
            && content.Height > 0
            && (content.Width != self.pool_size.Width || content.Height != self.pool_size.Height)
        {
            self.pool.Recreate(&self.device, FORMAT, 2, content)?;
            self.pool_size = content;
        }
        Ok(Some(out))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
