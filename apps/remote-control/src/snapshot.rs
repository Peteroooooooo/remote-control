//! Viewer test aid (`shot` in `view --script`): saves the last decoded
//! picture as a half-size BMP, so tests can see what the viewer shows
//! without capturing this PC's screen. enc-test saves full-size crops with it.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Graphics::{
    Direct3D11::{D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING},
    Dxgi::Common::{DXGI_FORMAT_AYUV, DXGI_SAMPLE_DESC},
};

use crate::{decode::Picture, gpu::Gpu};

pub fn save_bmp(gpu: &Gpu, pic: &Picture, size: (u32, u32), path: &Path) -> Result<()> {
    save_region(gpu, pic, (0, 0, size.0, size.1), 2, path)
}

/// Saves `w` x `h` pixels from (`x`, `y`) of the picture, every `step`th
/// pixel (1 = full size). NV12 and AYUV pictures.
pub fn save_region(gpu: &Gpu, pic: &Picture, (x0, y0, rw, rh): (u32, u32, u32, u32), step: u32, path: &Path) -> Result<()> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        pic.tex.GetDesc(&mut desc);
        let sd = D3D11_TEXTURE2D_DESC {
            MipLevels: 1,
            ArraySize: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut st = None;
        gpu.device.CreateTexture2D(&sd, None, Some(&mut st))?;
        let st = st.context("staging texture")?;
        gpu.ctx.CopySubresourceRegion(&st, 0, 0, 0, 0, &pic.tex, pic.slice * desc.MipLevels, None);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let pitch = m.RowPitch as usize;
        let base = m.pData as *const u8;
        let ayuv = desc.Format == DXGI_FORMAT_AYUV;
        let plane = pitch * desc.Height as usize;
        let mem = std::slice::from_raw_parts(base, if ayuv { plane } else { plane * 3 / 2 });
        // (Y, U, V) at a pixel. AYUV is V, U, Y, A in memory.
        let yuv = |x: usize, y: usize| -> (u8, u8, u8) {
            if ayuv {
                let p = &mem[y * pitch + 4 * x..];
                (p[2], p[1], p[0])
            } else {
                let c = plane + (y / 2) * pitch + (x / 2) * 2;
                (mem[y * pitch + x], mem[c], mem[c + 1])
            }
        };
        let s = step.max(1) as usize;
        let (x0, y0) = (x0 as usize, y0 as usize);
        let w = (rw as usize).min(desc.Width as usize - x0.min(desc.Width as usize)) / s;
        let h = (rh as usize).min(desc.Height as usize - y0.min(desc.Height as usize)) / s;
        let row = (w * 3).div_ceil(4) * 4;
        let mut px = vec![0u8; row * h];
        for y in 0..h {
            // BMP rows go bottom to top.
            let out = &mut px[(h - 1 - y) * row..][..w * 3];
            for x in 0..w {
                let (yv, u, v) = yuv(x0 + s * x, y0 + s * y);
                let (yv, u, v) = (yv as f32, u as f32 - 128.0, v as f32 - 128.0);
                // BT.709, limited range (what the host's video processor writes).
                let c = 1.164 * (yv - 16.0);
                let clamp = |f: f32| f.clamp(0.0, 255.0) as u8;
                out[3 * x] = clamp(c + 2.112 * u);
                out[3 * x + 1] = clamp(c - 0.213 * u - 0.533 * v);
                out[3 * x + 2] = clamp(c + 1.793 * v);
            }
        }
        gpu.ctx.Unmap(&st, 0);
        let mut f = Vec::with_capacity(54 + px.len());
        f.extend_from_slice(b"BM");
        f.extend_from_slice(&((54 + px.len()) as u32).to_le_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(&54u32.to_le_bytes());
        f.extend_from_slice(&40u32.to_le_bytes());
        f.extend_from_slice(&(w as i32).to_le_bytes());
        f.extend_from_slice(&(h as i32).to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&24u16.to_le_bytes());
        f.extend_from_slice(&[0u8; 24]);
        f.extend_from_slice(&px);
        std::fs::write(path, f).with_context(|| format!("write {}", path.display()))
    }
}
