//! Direct3D 11 device on the adapter that drives the captured monitor.

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};

pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub vendor: u32,
    pub name: String,
}

struct Candidate {
    adapter: IDXGIAdapter1,
    vendor: u32,
    name: String,
    owns_monitor: bool,
}

fn vendor_rank(vendor: u32) -> u32 {
    match vendor {
        VENDOR_NVIDIA => 0,
        VENDOR_AMD => 1,
        VENDOR_INTEL => 2,
        _ => 3,
    }
}

/// Picks the adapter whose output is `monitor` (raw HMONITOR), so that frames never
/// cross between GPUs. `force_vendor` overrides that for testing another encoder.
pub fn create_gpu(monitor: isize, force_vendor: Option<u32>) -> Result<Gpu> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut found: Vec<Candidate> = Vec::new();
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let desc = adapter.GetDesc1()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            let mut owns_monitor = false;
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                j += 1;
                if let Ok(od) = output.GetDesc() {
                    if od.Monitor.0 as isize == monitor {
                        owns_monitor = true;
                    }
                }
            }
            found.push(Candidate { adapter, vendor: desc.VendorId, name, owns_monitor });
        }

        let pick = match force_vendor {
            Some(v) => found.into_iter().find(|c| c.vendor == v),
            None => {
                found.sort_by_key(|c| (!c.owns_monitor, vendor_rank(c.vendor)));
                found.into_iter().next()
            }
        }
        .ok_or_else(|| anyhow!("no suitable graphics adapter found"))?;

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &pick.adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.ok_or_else(|| anyhow!("D3D11 device was not created"))?;
        let context = context.ok_or_else(|| anyhow!("D3D11 context was not created"))?;

        // The encoder touches the device from its own threads.
        let mt: ID3D11Multithread = context.cast()?;
        let _ = mt.SetMultithreadProtected(true);

        Ok(Gpu { device, context, vendor: pick.vendor, name: pick.name })
    }
}
