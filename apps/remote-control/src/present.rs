//! Shows decoded frames in the viewer window: a flip-model swap chain, the
//! video processor converting NV12 straight into the back buffer, and
//! presenting without waiting for vsync (tearing allowed) for the lowest delay.

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            Direct3D11::ID3D11Texture2D,
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
                DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
                DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
                DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice1, IDXGIFactory2,
                IDXGIFactory5, IDXGISwapChain1,
            },
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
    core::Interface,
};

use crate::{
    decode::Picture,
    gpu::Gpu,
    vp::{self, Vp},
};

pub struct Presenter {
    hwnd: HWND,
    swap: IDXGISwapChain1,
    tearing: bool,
    size: (u32, u32),
    vp: Option<Vp>,
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
}

impl Presenter {
    pub fn new(gpu: &Gpu, hwnd: HWND) -> Result<Presenter> {
        unsafe {
            let factory: IDXGIFactory2 = gpu.adapter.GetParent()?;
            let mut allow = 0i32;
            let tearing = factory
                .cast::<IDXGIFactory5>()
                .and_then(|f| {
                    f.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut i32 as *mut _, 4)
                })
                .is_ok()
                && allow != 0;
            let size = client_size(hwnd);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 },
                ..Default::default()
            };
            let swap = factory
                .CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)
                .context("CreateSwapChainForHwnd")?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            if let Ok(d) = gpu.device.cast::<IDXGIDevice1>() {
                let _ = d.SetMaximumFrameLatency(1);
            }
            Ok(Presenter { hwnd, swap, tearing, size, vp: None })
        }
    }

    /// Draws `pic` (showing the top-left `stream` pixels of it) letterboxed into the window.
    pub fn present(&mut self, gpu: &Gpu, pic: &Picture, stream: (u32, u32)) -> Result<()> {
        unsafe {
            let now = client_size(self.hwnd);
            if now != self.size {
                self.vp = None;
                let flags = if self.tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
                self.swap.ResizeBuffers(0, now.0, now.1, DXGI_FORMAT_UNKNOWN, flags).context("ResizeBuffers")?;
                self.size = now;
            }
            if self.vp.as_ref().is_none_or(|v| v.input != stream) {
                self.vp = Some(Vp::new(gpu, stream, self.size, vp::YUV_709, vp::RGB_FULL)?);
            }
            let vp = self.vp.as_ref().expect("set above");
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            vp.blt(&pic.tex, pic.slice, &back, vp::letterbox(stream, self.size))?;
            drop(back);
            let flags = if self.tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
            self.swap.Present(0, flags).ok().context("Present")
        }
    }

    /// The window's client area no longer matches the back buffers.
    pub fn resized(&self) -> bool {
        client_size(self.hwnd) != self.size
    }
}
