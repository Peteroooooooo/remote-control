//! DXGI desktop duplication of one output. Each new screen image is copied
//! into a texture we own, so it can be encoded later (or again, for a key
//! frame) after the duplication frame has been released.

use anyhow::{Context, Result};
use windows::{
    Win32::Graphics::{
        Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            ID3D11Texture2D,
        },
        Dxgi::{
            DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1,
            IDXGIOutputDuplication, IDXGIResource,
        },
    },
    core::Interface,
};

use crate::gpu::Gpu;

pub enum Grab {
    /// A new image is in `Capture::image`. `present_qpc` is when the screen
    /// was updated, in QueryPerformanceCounter ticks.
    New { present_qpc: i64 },
    Timeout,
    /// Desktop switch, mode change or similar: create a new `Capture`.
    Lost,
}

pub struct Capture {
    dupl: IDXGIOutputDuplication,
    pub image: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Degrees clockwise the image must be turned to look like the display:
    /// a rotated display is duplicated in its unrotated (panel) orientation.
    pub rotation: u16,
}

impl Capture {
    pub fn new(gpu: &Gpu, output: &IDXGIOutput1) -> Result<Capture> {
        unsafe {
            let dupl = output.DuplicateOutput(&gpu.device).context("DuplicateOutput")?;
            let dd = dupl.GetDesc();
            let mode = dd.ModeDesc;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mode.Width,
                Height: mode.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut image = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut image))?;
            Ok(Capture {
                dupl,
                image: image.context("CreateTexture2D")?,
                width: mode.Width,
                height: mode.Height,
                rotation: crate::monitors::rotation_degrees(dd.Rotation),
            })
        }
    }

    /// The image's size once turned upright.
    pub fn upright_size(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 { (self.height, self.width) } else { (self.width, self.height) }
    }

    pub fn grab(&mut self, gpu: &Gpu, timeout_ms: u32) -> Result<Grab> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Timeout),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(Grab::Lost),
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
            // LastPresentTime 0 means only the mouse pointer moved.
            let fresh = info.LastPresentTime != 0;
            if fresh && let Some(r) = res {
                let tex: ID3D11Texture2D = r.cast()?;
                gpu.ctx.CopyResource(&self.image, &tex);
            }
            let _ = self.dupl.ReleaseFrame();
            Ok(if fresh { Grab::New { present_qpc: info.LastPresentTime } } else { Grab::Timeout })
        }
    }
}
