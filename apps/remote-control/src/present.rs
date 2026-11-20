//! Shows decoded frames in the viewer window: a flip-model swap chain, a
//! pixel shader (`torgb.rs`) drawing the picture into the back buffer, and
//! Present. If the shader cannot run, the video processor draws instead.
//!
//! In full screen the frame goes to the screen directly (the compositor
//! skips the window), at the next refresh, a newer frame replacing one still
//! waiting: measured 2026-11-20 on a 120 Hz laptop, 11 ms from Present to
//! the screen and no frame lost, against 15 ms and 11% in a window. Tearing
//! is never asked for: in full screen it would cut the picture across.
//!
//! In a window the desktop compositor takes the newest frame at each
//! refresh, so two frames within one refresh lose the first. There each
//! frame waits for a refresh of its own, Present holding the render thread
//! while the frame before still waits (one refresh at most; frames that came
//! meanwhile give way to the newest). Not when frames come faster than the
//! screen shows them: then every frame would wait a refresh and some would
//! be lost all the same.

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D},
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
                DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
                DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice1, IDXGIFactory2,
                IDXGISwapChain1,
            },
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
    core::Interface,
};

use crate::{
    decode::Picture,
    gpu::Gpu,
    torgb::ToRgb,
    vp::{self, Vp},
};

pub struct Presenter {
    hwnd: HWND,
    swap: IDXGISwapChain1,
    size: (u32, u32),
    /// None once it failed: the video processor draws from then on.
    shader: Option<ToRgb>,
    rtv: Option<ID3D11RenderTargetView>,
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
                ..Default::default()
            };
            let swap = factory
                .CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)
                .context("CreateSwapChainForHwnd")?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            if let Ok(d) = gpu.device.cast::<IDXGIDevice1>() {
                let _ = d.SetMaximumFrameLatency(1);
            }
            let shader = ToRgb::new(gpu).inspect_err(|e| crate::log!("picture shader: {e:#}; the video processor draws")).ok();
            Ok(Presenter { hwnd, swap, size, shader, rtv: None, vp: None })
        }
    }

    /// Draws `pic` (showing the top-left `stream` pixels of it) letterboxed into the window.
    pub fn present(&mut self, gpu: &Gpu, pic: &Picture, stream: (u32, u32)) -> Result<()> {
        unsafe {
            let now = client_size(self.hwnd);
            if now != self.size {
                self.vp = None;
                self.rtv = None;
                self.swap.ResizeBuffers(0, now.0, now.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)).context("ResizeBuffers")?;
                self.size = now;
            }
            let dest = vp::letterbox(stream, self.size);
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            if !self.shade(gpu, pic, stream, &back, dest) {
                if self.vp.as_ref().is_none_or(|v| v.input != stream) {
                    self.vp = Some(Vp::new(gpu, stream, self.size, vp::YUV_709, vp::RGB_FULL)?);
                }
                self.vp.as_ref().expect("set above").blt(&pic.tex, pic.slice, &back, dest)?;
            }
            drop(back);
            let own_refresh = !crate::window::fullscreen() && crate::quality::frames_fit_screen();
            self.swap.Present(u32::from(own_refresh), DXGI_PRESENT(0)).ok().context("Present")
        }
    }

    /// Draws with the shader; false if there is none (any more).
    fn shade(&mut self, gpu: &Gpu, pic: &Picture, stream: (u32, u32), back: &ID3D11Texture2D, dest: RECT) -> bool {
        let Some(shader) = self.shader.as_mut() else { return false };
        let r = (|| {
            if self.rtv.is_none() {
                let mut v = None;
                unsafe { gpu.device.CreateRenderTargetView(back, None, Some(&mut v)) }.context("CreateRenderTargetView")?;
                self.rtv = v;
            }
            shader.draw(gpu, pic, stream, self.rtv.as_ref().context("no render target")?, dest)
        })();
        if let Err(e) = r {
            crate::log!("picture shader: {e:#}; the video processor draws");
            self.shader = None;
            self.rtv = None;
            return false;
        }
        true
    }

    /// The window's client area no longer matches the back buffers.
    pub fn resized(&self) -> bool {
        client_size(self.hwnd) != self.size
    }
}
