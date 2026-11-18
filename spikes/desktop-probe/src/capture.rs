//! DXGI desktop duplication of the primary output, read back to CPU memory.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use windows::{
    Win32::Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
            ID3D11Texture2D,
        },
        Dxgi::{
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1,
            IDXGIOutputDuplication, IDXGIResource,
        },
    },
    core::Interface,
};

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA, tightly packed.
    pub rgba: Vec<u8>,
}

pub struct Capture {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    last: Option<Image>,
}

impl Capture {
    /// Must be called on a thread attached to the current input desktop.
    pub fn new() -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.context("no device")?;
            let ctx = ctx.context("no context")?;
            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let output = adapter.EnumOutputs(0).context("EnumOutputs(0)")?;
            let dupl = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device).context("DuplicateOutput")?;
            Ok(Self { device, ctx, dupl, staging: None, last: None })
        }
    }

    /// Waits up to `timeout` for a new frame. Returns the latest image (new or
    /// previous); an error means the duplication is lost and must be recreated.
    pub fn grab(&mut self, timeout: Duration) -> Result<Option<&Image>> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dupl.AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(self.last.as_ref()),
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
            let r = self.copy_out(res.context("no frame resource")?);
            let _ = self.dupl.ReleaseFrame();
            r?;
            Ok(self.last.as_ref())
        }
    }

    unsafe fn copy_out(&mut self, res: IDXGIResource) -> Result<()> {
        unsafe {
            let tex: ID3D11Texture2D = res.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            if self.staging.is_none() {
                let mut sd = desc;
                sd.Usage = D3D11_USAGE_STAGING;
                sd.BindFlags = 0;
                sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                sd.MiscFlags = 0;
                sd.MipLevels = 1;
                sd.ArraySize = 1;
                let mut st = None;
                self.device.CreateTexture2D(&sd, None, Some(&mut st))?;
                self.staging = st;
            }
            let st = self.staging.as_ref().context("staging")?;
            self.ctx.CopyResource(st, &tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let (w, h) = (desc.Width as usize, desc.Height as usize);
            let mut rgba = vec![0u8; w * h * 4];
            for y in 0..h {
                let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), w * 4);
                for (dst, src) in rgba[y * w * 4..(y + 1) * w * 4].chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                    dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
                }
            }
            self.ctx.Unmap(st, 0);
            self.last = Some(Image { width: w as u32, height: h as u32, rgba });
            Ok(())
        }
    }
}

/// Writes shots/<timestamp>_<desktop>.png and keeps the newest 200 files.
pub fn save_png(img: &Image, desktop: &str) -> Result<()> {
    let dir = crate::shots_dir();
    std::fs::create_dir_all(&dir)?;
    let safe: String = desktop.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let path: PathBuf = dir.join(format!("{}_{safe}.png", crate::timestamp()));
    let f = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = png::Encoder::new(f, img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(&img.rgba)?;
    let mut files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
        .map(|e| e.path())
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(200);
    for p in &files[..excess] {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}
