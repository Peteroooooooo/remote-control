//! D3D11 device setup shared by capture, encode, decode and display.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
            },
            Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1},
        },
    },
    core::Interface,
};

pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter1,
    pub luid: LUID,
    pub name: String,
}

impl Gpu {
    /// A device on `adapter` that video encoders, decoders and the video
    /// processor can use from several threads.
    pub fn new(adapter: IDXGIAdapter1) -> Result<Gpu> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.context("no device")?;
            let ctx = ctx.context("no context")?;
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
            // Ask WDDM for high GPU scheduling priority (like Sunshine/OBS) so
            // hardware encode/decode is not throttled behind background work.
            if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
                let _ = dxgi.SetGPUThreadPriority(7);
            }
            let desc = adapter.GetDesc1()?;
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            Ok(Gpu { device, ctx, adapter, luid: desc.AdapterLuid, name })
        }
    }

    /// The adapter that drives the main display (the one at desktop 0,0), and that output.
    pub fn for_primary_output() -> Result<(Gpu, IDXGIOutput1, RECT)> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                let mut j = 0;
                while let Ok(output) = adapter.EnumOutputs(j) {
                    j += 1;
                    let d = output.GetDesc()?;
                    let r = d.DesktopCoordinates;
                    if d.AttachedToDesktop.as_bool() && r.left == 0 && r.top == 0 {
                        return Ok((Gpu::new(adapter)?, output.cast()?, r));
                    }
                }
            }
            bail!("{}", crate::monitors::NO_DISPLAY)
        }
    }

    /// The first hardware adapter (for the viewer's decoder and window).
    pub fn default_adapter() -> Result<Gpu> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            Gpu::new(factory.EnumAdapters1(0).context("no graphics adapter")?)
        }
    }
}
