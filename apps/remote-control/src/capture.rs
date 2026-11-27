//! Capture of one display. Each new screen image is copied into a texture we
//! own, so it can be encoded later (or again, for a key frame) after the
//! capture's frame has gone back.
//!
//! On the user's desktop the image comes from Windows.Graphics.Capture with
//! the pointer left out (`wgc`): in this process, or in the service's helper
//! from an agent running as the console user. On the Winlogon desktop (lock,
//! login and UAC screens), or when WGC fails, from DXGI desktop duplication,
//! which draws the pointer into the image while a window is dragged.

use anyhow::{Context, Result};
use windows::{
    Win32::Graphics::{
        Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            ID3D11Texture2D,
        },
        Dxgi::{
            Common::{
                DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
            },
            DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1,
            IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
        },
    },
    core::Interface,
};

use crate::{
    gpu::Gpu,
    wgc::{Agent, Wgc},
};

pub enum Grab {
    /// A new image is in `Capture::image`. `present_qpc` is when the screen
    /// was updated, in QueryPerformanceCounter ticks.
    New { present_qpc: i64 },
    Timeout,
    /// Desktop switch, mode change or similar: create a new `Capture`.
    Lost,
}

enum Source {
    Dxgi(IDXGIOutputDuplication),
    Wgc(Wgc),
    Agent(Agent),
}

pub struct Capture {
    src: Source,
    image: Option<ID3D11Texture2D>,
    /// `image` holds a grabbed image.
    grabbed: bool,
    pub width: u32,
    pub height: u32,
    /// Degrees clockwise the image must be turned to look like the display:
    /// a rotated display is duplicated in its unrotated (panel) orientation
    /// (WGC's image is upright).
    pub rotation: u16,
    /// Display HDR status and SDR white-level scale (`1.0 = 80 nits`).
    pub hdr_info: crate::hdr::DisplayHdrInfo,
    /// Whether captured textures are `DXGI_FORMAT_R16G16B16A16_FLOAT` scRGB
    /// (HDR) instead of `DXGI_FORMAT_B8G8R8A8_UNORM` (SDR).
    pub fp16: bool,
}

/// A texture with the requested format that GPU shaders and video processors can read.
pub fn surface_texture(gpu: &Gpu, w: u32, h: u32, format: DXGI_FORMAT, misc: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: misc,
    };
    let mut t = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.context("CreateTexture2D")
}

/// A BGRA texture the video processor can read and write.
pub fn bgra_texture(gpu: &Gpu, w: u32, h: u32, misc: u32) -> Result<ID3D11Texture2D> {
    surface_texture(gpu, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, misc)
}

/// A 16-bit float scRGB texture for HDR desktop capture.
pub fn fp16_texture(gpu: &Gpu, w: u32, h: u32, misc: u32) -> Result<ID3D11Texture2D> {
    surface_texture(gpu, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, misc)
}

/// The display's device name (`\\.\DISPLAY1`).
pub fn device_name(output: &IDXGIOutput1) -> Result<String> {
    let d = unsafe { output.GetDesc()? };
    let len = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
    Ok(String::from_utf16_lossy(&d.DeviceName[..len]))
}

impl Capture {
    /// DXGI desktop duplication. Uses `DuplicateOutput1` with FP16 support when
    /// HDR is active on the display so HDR desktops are captured in linear scRGB
    /// and tone-mapped on the GPU without clipping or washed-out colors.
    pub fn dxgi(gpu: &Gpu, output: &IDXGIOutput1) -> Result<Capture> {
        let hdr_info = device_name(output)
            .map(|n| crate::hdr::query_display_hdr(&n))
            .unwrap_or_else(|_| crate::hdr::DisplayHdrInfo::sdr());
        unsafe {
            let dupl = if hdr_info.hdr_enabled
                && let Ok(out5) = output.cast::<IDXGIOutput5>()
            {
                let fmts = [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];
                out5.DuplicateOutput1(&gpu.device, 0, &fmts)
                    .or_else(|_| output.DuplicateOutput(&gpu.device))
                    .context("DuplicateOutput")?
            } else {
                output.DuplicateOutput(&gpu.device).context("DuplicateOutput")?
            };
            let dd = dupl.GetDesc();
            let (w, h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let fp16 = dd.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
            Ok(Capture {
                src: Source::Dxgi(dupl),
                image: None,
                grabbed: false,
                width: w,
                height: h,
                rotation: crate::monitors::rotation_degrees(dd.Rotation),
                hdr_info,
                fp16,
            })
        }
    }

    /// WGC in this process (it must not be SYSTEM).
    pub fn wgc(gpu: &Gpu, output: &IDXGIOutput1) -> Result<Capture> {
        let hdr_info = device_name(output)
            .map(|n| crate::hdr::query_display_hdr(&n))
            .unwrap_or_else(|_| crate::hdr::DisplayHdrInfo::sdr());
        let monitor = unsafe { output.GetDesc()? }.Monitor;
        let w = Wgc::start(gpu, monitor, hdr_info.hdr_enabled)?;
        let (width, height, fp16) = (w.width, w.height, w.fp16);
        Ok(Capture {
            src: Source::Wgc(w),
            image: None,
            grabbed: false,
            width,
            height,
            rotation: 0,
            hdr_info,
            fp16,
        })
    }

    /// WGC in an agent running as the console user (for the service's helper).
    /// `prev`: the last capture's agent, moved to this display when it still runs.
    pub fn agent(gpu: &Gpu, output: &IDXGIOutput1, prev: Option<Agent>) -> Result<Capture> {
        let dev = device_name(output)?;
        let hdr_info = crate::hdr::query_display_hdr(&dev);
        let a = Agent::open(gpu, &dev, prev)?;
        let (width, height, fp16) = (a.width, a.height, a.fp16);
        Ok(Capture {
            src: Source::Agent(a),
            image: None,
            grabbed: false,
            width,
            height,
            rotation: 0,
            hdr_info,
            fp16,
        })
    }

    /// Single-window WGC in this process (`CreateForWindow`).
    pub fn wgc_window(gpu: &Gpu, hwnd_raw: u64) -> Result<Capture> {
        let hwnd = windows::Win32::Foundation::HWND(hwnd_raw as usize as *mut _);
        let w = Wgc::start_window(gpu, hwnd)?;
        let (width, height, fp16) = (w.width, w.height, w.fp16);
        Ok(Capture {
            src: Source::Wgc(w),
            image: None,
            grabbed: false,
            width,
            height,
            rotation: 0,
            hdr_info: crate::hdr::DisplayHdrInfo::sdr(),
            fp16,
        })
    }

    /// Single-window WGC in the console-user capture agent (for the service's helper).
    pub fn agent_window(gpu: &Gpu, output: &IDXGIOutput1, hwnd_raw: u64, prev: Option<Agent>) -> Result<Capture> {
        let dev = device_name(output)?;
        let target = format!("window:{hwnd_raw}|{dev}");
        let a = Agent::open(gpu, &target, prev)?;
        let (width, height, fp16) = (a.width, a.height, a.fp16);
        Ok(Capture {
            src: Source::Agent(a),
            image: None,
            grabbed: false,
            width,
            height,
            rotation: 0,
            hdr_info: crate::hdr::DisplayHdrInfo::sdr(),
            fp16,
        })
    }

    /// Staging texture holding the last grabbed frame, allocated on first use
    /// in the matching pixel format (`R16G16B16A16_FLOAT` for HDR, `B8G8R8A8_UNORM` for SDR).
    pub fn image(&mut self, gpu: &Gpu) -> Result<&ID3D11Texture2D> {
        if self.image.is_none() {
            let tex = if self.fp16 {
                fp16_texture(gpu, self.width, self.height, 0)?
            } else {
                bgra_texture(gpu, self.width, self.height, 0)?
            };
            self.image = Some(tex);
        }
        Ok(self.image.as_ref().expect("created above"))
    }

    /// The agent behind this capture, kept for the next one (see `Agent::open`).
    pub fn into_agent(self) -> Option<Agent> {
        match self.src {
            Source::Agent(a) => Some(a),
            _ => None,
        }
    }

    /// Whether the image comes from WGC (without the pointer).
    pub fn is_wgc(&self) -> bool {
        !matches!(self.src, Source::Dxgi(_))
    }

    /// What the image comes from, for the log.
    pub fn kind(&self) -> &'static str {
        match self.src {
            Source::Dxgi(_) => "DXGI duplication",
            Source::Wgc(_) => "WGC",
            Source::Agent(_) => "WGC in the user's agent",
        }
    }

    /// The image's size once turned upright.
    pub fn upright_size(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 { (self.height, self.width) } else { (self.width, self.height) }
    }

    /// Shared ring textures when capturing via `wgc::Agent`, so shaders can
    /// read directly from the acquired slot without copying into `self.image`.
    pub fn agent_ring(&self) -> Option<Vec<ID3D11Texture2D>> {
        match &self.src {
            Source::Agent(a) => Some(a.ring_textures()),
            _ => None,
        }
    }

    /// Grabs directly from `wgc::Agent` without copying into `self.image`.
    pub fn grab_agent_with(&mut self, timeout_ms: u32, on_slot: impl FnMut(usize)) -> Option<Result<Grab>> {
        match &self.src {
            Source::Agent(a) => Some(a.grab_with(timeout_ms, on_slot)),
            _ => None,
        }
    }

    /// For a new picture from this capture: a still screen brings no new
    /// image, so the last one is used again. With `wgc::Agent` straight from
    /// its slot (`on_slot`, as in `grab_agent_with`), else it is still in
    /// `image`. False if there is none (a new one comes with the next update).
    pub fn again(&self, on_slot: impl FnOnce(usize)) -> bool {
        match &self.src {
            Source::Agent(a) => a.again_with(on_slot),
            _ => false,
        }
    }

    /// `image` holds the last grabbed image (see `again`).
    pub fn has_image(&self) -> bool {
        self.grabbed
    }

    pub fn grab(&mut self, gpu: &Gpu, timeout_ms: u32) -> Result<Grab> {
        if self.image.is_none() {
            let tex = if self.fp16 {
                fp16_texture(gpu, self.width, self.height, 0)?
            } else {
                bgra_texture(gpu, self.width, self.height, 0)?
            };
            self.image = Some(tex);
        }
        let image = self.image.as_ref().expect("created above");
        let expected_fp16 = self.fp16;
        let g = match &self.src {
            Source::Dxgi(dupl) => grab_dxgi(dupl, image, expected_fp16, gpu, timeout_ms),
            Source::Wgc(w) => w.grab(gpu, image, timeout_ms),
            Source::Agent(a) => a.grab(gpu, image, timeout_ms),
        };
        if matches!(g, Ok(Grab::New { .. })) {
            self.grabbed = true;
        }
        g
    }
}

struct FrameGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseFrame();
        }
    }
}

fn grab_dxgi(
    dupl: &IDXGIOutputDuplication,
    image: &ID3D11Texture2D,
    expected_fp16: bool,
    gpu: &Gpu,
    timeout_ms: u32,
) -> Result<Grab> {
    unsafe {
        let acquired = {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match dupl.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Timeout),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(Grab::Lost),
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
            (info, res)
        };
        // `_guard` is declared before `(info, res)`, so `res` and `tex` are always
        // released before `ReleaseFrame()` runs—including on error return.
        let _guard = FrameGuard(dupl);
        let (info, res) = acquired;
        // LastPresentTime 0 means only the mouse pointer moved.
        let fresh = info.LastPresentTime != 0;
        if fresh && let Some(r) = res {
            let tex: ID3D11Texture2D = r.cast()?;
            let mut td = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut td);
            let actual_fp16 = td.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
            if actual_fp16 != expected_fp16 {
                // Display toggled HDR/SDR or driver switched surface format:
                // rebuild capture + shader views cleanly instead of silently
                // failing CopyResource due to mismatched formats.
                return Ok(Grab::Lost);
            }
            gpu.ctx.CopyResource(image, &tex);
        }
        Ok(if fresh { Grab::New { present_qpc: info.LastPresentTime } } else { Grab::Timeout })
    }
}
