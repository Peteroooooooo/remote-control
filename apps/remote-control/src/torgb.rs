//! Viewer: the decoded picture (NV12, or AYUV for 4:4:4) into the BGRA back
//! buffer, scaled and letterboxed, with a pixel shader on the 3D engine.
//!
//! The D3D11 video processor did this before. On Intel Arc it runs on the
//! video engine next to the decoder and took 5-6 ms a frame at 2880x1920
//! (measured 2026-11-20 with PresentMon: the video engine busy 8.4 ms a
//! frame with a 1440p stream, 6.6 ms with 720p), so at 120 frames a second
//! the engine was nearly full and every frame reached the screen 8 ms late.
//! The shader takes well under a millisecond on any D3D11 GPU.
//!
//! Colours as the host makes them (`nv12.rs`, `yuv444.rs`): BT.709, limited
//! range, 4:2:0 chroma sited left. Scaling: exact at 1:1, Catmull-Rom on
//! luma when enlarging, 2x2 or 4x4 samples per pixel when shrinking (so
//! text does not shimmer).

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_TEXTURE2DARRAY, Fxc::D3DCompile, ID3DBlob},
            Direct3D11::{
                D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_CULL_NONE,
                D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RASTERIZER_DESC, D3D11_SAMPLER_DESC,
                D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_ARRAY_SRV,
                D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer,
                ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState,
                ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
            },
            Dxgi::Common::{
                DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
                DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
            },
        },
    },
    core::{Interface, PCSTR, s},
};

use crate::{decode::Picture, gpu::Gpu};

const HLSL: &str = r#"
Texture2DArray<float>  t_y    : register(t0);
Texture2DArray<float2> t_uv   : register(t1);
Texture2DArray<float4> t_ayuv : register(t2);
SamplerState smp : register(s0);

cbuffer consts : register(b0) {
    float2 src_px;   // the picture's visible size, in its pixels
    float2 inv_tex;  // 1 / the texture's size (it may be larger than the picture)
    float2 dst_off;  // top left of the picture in the back buffer
    float2 scale;    // picture pixels per back buffer pixel
    float  mode;     // 0 exact or bilinear, 1 enlarge, 2 shrink 2x2, 3 shrink 4x4
    float3 pad;
};

float4 vs_main(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

float3 to_rgb(float3 yuv) {
    float y = (yuv.x - 16.0 / 255.0) * (255.0 / 219.0);
    float u = (yuv.y - 128.0 / 255.0) * (255.0 / 224.0);
    float v = (yuv.z - 128.0 / 255.0) * (255.0 / 224.0);
    return saturate(float3(y + 1.5748 * v, y - 0.1873 * u - 0.4681 * v, y + 1.8556 * u));
}

// Picture pixel coordinates (pixel i's centre at i + 0.5), kept inside the
// visible picture: the texture's padding holds anything.
float2 inside(float2 p, float lo) { return clamp(p, lo, src_px - lo); }

float luma(float2 p) { return t_y.SampleLevel(smp, float3(inside(p, 0.5) * inv_tex, 0), 0); }

// Chroma sample j sits on luma pixel 2j (left sited), the texture puts it half
// a luma pixel further right: sample there. Its texel centres are 1 luma
// pixel in from the edges.
float2 chroma(float2 p) { return t_uv.SampleLevel(smp, float3(inside(p + float2(0.5, 0.0), 1.0) * inv_tex, 0), 0); }

float3 nv12(float2 p) { return float3(luma(p), chroma(p)); }

// AYUV in memory is V, U, Y, A: the view's R, G, B, A.
float3 ayuv(float2 p) { return t_ayuv.SampleLevel(smp, float3(inside(p, 0.5) * inv_tex, 0), 0).bgr; }

// Catmull-Rom from 5 bilinear samples: the weights folded into positions, the
// four corners (which weigh little) left out.
void cr_taps(float2 p, out float2 t0, out float2 t12, out float2 t3, out float2 w0, out float2 w12, out float2 w3) {
    float2 t1 = floor(p - 0.5) + 0.5;
    float2 f = p - t1;
    w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    w3 = f * f * (-0.5 + 0.5 * f);
    w12 = w1 + w2;
    t0 = t1 - 1.0;
    t12 = t1 + w2 / w12;
    t3 = t1 + 2.0;
}

float luma_cr(float2 p) {
    float2 t0, t12, t3, w0, w12, w3;
    cr_taps(p, t0, t12, t3, w0, w12, w3);
    float wt = w12.x * w0.y, wl = w0.x * w12.y, wm = w12.x * w12.y, wr = w3.x * w12.y, wb = w12.x * w3.y;
    float s = luma(float2(t12.x, t0.y)) * wt + luma(float2(t0.x, t12.y)) * wl + luma(t12) * wm
            + luma(float2(t3.x, t12.y)) * wr + luma(float2(t12.x, t3.y)) * wb;
    return s / (wt + wl + wm + wr + wb);
}

float3 ayuv_cr(float2 p) {
    float2 t0, t12, t3, w0, w12, w3;
    cr_taps(p, t0, t12, t3, w0, w12, w3);
    float wt = w12.x * w0.y, wl = w0.x * w12.y, wm = w12.x * w12.y, wr = w3.x * w12.y, wb = w12.x * w3.y;
    float3 s = ayuv(float2(t12.x, t0.y)) * wt + ayuv(float2(t0.x, t12.y)) * wl + ayuv(t12) * wm
             + ayuv(float2(t3.x, t12.y)) * wr + ayuv(float2(t12.x, t3.y)) * wb;
    return s / (wt + wl + wm + wr + wb);
}

float2 src_pos(float4 pos) { return (pos.xy - dst_off) * scale; }

float4 ps_nv12(float4 pos : SV_Position) : SV_Target {
    float2 p = src_pos(pos);
    float3 yuv;
    if (mode < 0.5) {
        yuv = nv12(p);
    } else if (mode < 1.5) {
        yuv = float3(luma_cr(p), chroma(p));
    } else {
        int n = mode < 2.5 ? 2 : 4;
        yuv = 0;
        for (int j = 0; j < n; j++)
            for (int i = 0; i < n; i++)
                yuv += nv12(p + (float2(i, j) + 0.5 - n * 0.5) / n * scale);
        yuv /= n * n;
    }
    return float4(to_rgb(yuv), 1.0);
}

float4 ps_ayuv(float4 pos : SV_Position) : SV_Target {
    float2 p = src_pos(pos);
    float3 yuv;
    if (mode < 0.5) {
        yuv = ayuv(p);
    } else if (mode < 1.5) {
        yuv = ayuv_cr(p);
    } else {
        int n = mode < 2.5 ? 2 : 4;
        yuv = 0;
        for (int j = 0; j < n; j++)
            for (int i = 0; i < n; i++)
                yuv += ayuv(p + (float2(i, j) + 0.5 - n * 0.5) / n * scale);
        yuv /= n * n;
    }
    return float4(to_rgb(yuv), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Default)]
struct Consts {
    src_px: [f32; 2],
    inv_tex: [f32; 2],
    dst_off: [f32; 2],
    scale: [f32; 2],
    mode: f32,
    pad: [f32; 3],
}

/// Shader views of one picture: luma and chroma planes, or the AYUV picture.
enum Views {
    Nv12(ID3D11ShaderResourceView, ID3D11ShaderResourceView),
    Ayuv(ID3D11ShaderResourceView),
}

/// A texture the shader can read, for decoders whose own textures it cannot.
struct Readable {
    tex: ID3D11Texture2D,
    key: (u32, u32, DXGI_FORMAT),
    views: Views,
}

pub struct ToRgb {
    vs: ID3D11VertexShader,
    ps_nv12: ID3D11PixelShader,
    ps_ayuv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rast: ID3D11RasterizerState,
    consts: ID3D11Buffer,
    last: Option<Consts>,
    /// Views of the decoder's textures: (texture, slice, the draw that last used them, views).
    cache: Vec<(usize, u32, u64, Views)>,
    draws: u64,
    copy: Option<Readable>,
}

/// Views unused for this many draws go: the decoder that made the texture is
/// gone (it cycles through its few textures much faster), and the views
/// would keep its textures alive.
const UNUSED_DRAWS: u64 = 64;

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        if let Err(e) =
            D3DCompile(HLSL.as_ptr() as *const _, HLSL.len(), PCSTR::null(), None, None, entry, target, 0, 0, &mut code, Some(&mut errors))
        {
            let msg = errors.map(|b| {
                let p = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                String::from_utf8_lossy(p).into_owned()
            });
            bail!("picture shader: {e} {}", msg.unwrap_or_default());
        }
        code.context("no shader code")
    }
}

fn bytes(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

/// How to sample: exact at 1:1, Catmull-Rom when enlarging, several samples per pixel when shrinking.
fn mode(ratio: f64) -> f32 {
    if (ratio - 1.0).abs() < 0.002 {
        0.0
    } else if ratio > 1.0 {
        1.0
    } else if ratio >= 0.5 {
        2.0
    } else {
        3.0
    }
}

impl ToRgb {
    pub fn new(gpu: &Gpu) -> Result<ToRgb> {
        unsafe {
            let vs_blob = compile(s!("vs_main"), s!("vs_5_0"))?;
            let nv12_blob = compile(s!("ps_nv12"), s!("ps_5_0"))?;
            let ayuv_blob = compile(s!("ps_ayuv"), s!("ps_5_0"))?;
            let mut vs = None;
            gpu.device.CreateVertexShader(bytes(&vs_blob), None, Some(&mut vs)).context("CreateVertexShader")?;
            let mut ps_nv12 = None;
            gpu.device.CreatePixelShader(bytes(&nv12_blob), None, Some(&mut ps_nv12)).context("CreatePixelShader(NV12)")?;
            let mut ps_ayuv = None;
            gpu.device.CreatePixelShader(bytes(&ayuv_blob), None, Some(&mut ps_ayuv)).context("CreatePixelShader(AYUV)")?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            gpu.device.CreateSamplerState(&sd, Some(&mut sampler)).context("CreateSamplerState")?;
            let rd = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                DepthClipEnable: false.into(),
                ..Default::default()
            };
            let mut rast = None;
            gpu.device.CreateRasterizerState(&rd, Some(&mut rast)).context("CreateRasterizerState")?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: size_of::<Consts>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut consts = None;
            gpu.device.CreateBuffer(&bd, None, Some(&mut consts)).context("CreateBuffer")?;
            Ok(ToRgb {
                vs: vs.context("no vs")?,
                ps_nv12: ps_nv12.context("no ps")?,
                ps_ayuv: ps_ayuv.context("no ps")?,
                sampler: sampler.context("no sampler")?,
                rast: rast.context("no rast")?,
                consts: consts.context("no buffer")?,
                last: None,
                cache: Vec::new(),
                draws: 0,
                copy: None,
            })
        }
    }

    fn views(gpu: &Gpu, tex: &ID3D11Texture2D, format: DXGI_FORMAT, slice: u32) -> Result<Views> {
        let srv = |f: DXGI_FORMAT| -> Result<ID3D11ShaderResourceView> {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: f,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_SRV { MostDetailedMip: 0, MipLevels: 1, FirstArraySlice: slice, ArraySize: 1 },
                },
            };
            let mut v = None;
            unsafe { gpu.device.CreateShaderResourceView(tex, Some(&desc), Some(&mut v)) }.context("CreateShaderResourceView")?;
            v.context("no view")
        };
        Ok(match format {
            DXGI_FORMAT_NV12 => Views::Nv12(srv(DXGI_FORMAT_R8_UNORM)?, srv(DXGI_FORMAT_R8G8_UNORM)?),
            DXGI_FORMAT_AYUV => Views::Ayuv(srv(DXGI_FORMAT_R8G8B8A8_UNORM)?),
            f => bail!("picture in format {}, not NV12 or AYUV", f.0),
        })
    }

    /// The views to read `pic` through: the decoder's own texture if shaders
    /// may read it, else a copy.
    fn views_for(&mut self, gpu: &Gpu, pic: &Picture, desc: &D3D11_TEXTURE2D_DESC) -> Result<&Views> {
        self.draws += 1;
        let now = self.draws;
        self.cache.retain(|e| now - e.2 < UNUSED_DRAWS);
        if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
            let key = pic.tex.as_raw() as usize;
            let at = match self.cache.iter().position(|e| e.0 == key && e.1 == pic.slice) {
                Some(i) => i,
                None => {
                    self.cache.push((key, pic.slice, now, Self::views(gpu, &pic.tex, desc.Format, pic.slice)?));
                    self.cache.len() - 1
                }
            };
            self.cache[at].2 = now;
            return Ok(&self.cache[at].3);
        }
        let key = (desc.Width, desc.Height, desc.Format);
        if self.copy.as_ref().is_none_or(|c| c.key != key) {
            let cd = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let mut t = None;
            unsafe { gpu.device.CreateTexture2D(&cd, None, Some(&mut t)) }.context("CreateTexture2D for the picture")?;
            let tex = t.context("no texture")?;
            let views = Self::views(gpu, &tex, desc.Format, 0)?;
            self.copy = Some(Readable { tex, key, views });
        }
        let c = self.copy.as_ref().expect("made above");
        unsafe { gpu.ctx.CopySubresourceRegion(&c.tex, 0, 0, 0, 0, &pic.tex, pic.slice, None) };
        Ok(&c.views)
    }

    /// Drops cached SRVs of decoder textures (e.g. when the decoder is rebuilt).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Draws the top-left `stream` pixels of `pic` into `dest` of the target;
    /// the rest of the target becomes black.
    pub fn draw(&mut self, gpu: &Gpu, pic: &Picture, stream: (u32, u32), rtv: &ID3D11RenderTargetView, dest: RECT) -> Result<()> {
        const NONE_SRVS: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { pic.tex.GetDesc(&mut desc) };
        let (dw, dh) = ((dest.right - dest.left).max(1) as f32, (dest.bottom - dest.top).max(1) as f32);
        let (sw, sh) = (stream.0.min(desc.Width).max(1) as f32, stream.1.min(desc.Height).max(1) as f32);
        let consts = Consts {
            src_px: [sw, sh],
            inv_tex: [1.0 / desc.Width.max(1) as f32, 1.0 / desc.Height.max(1) as f32],
            dst_off: [dest.left as f32, dest.top as f32],
            scale: [sw / dw, sh / dh],
            mode: mode((dw / sw).min(dh / sh) as f64),
            pad: [0.0; 3],
        };
        if self.last != Some(consts) {
            unsafe { gpu.ctx.UpdateSubresource(&self.consts, 0, None, &consts as *const _ as *const _, 0, 0) };
            self.last = Some(consts);
        }
        let (nv12, srvs, count) = match self.views_for(gpu, pic, &desc)? {
            Views::Nv12(y, uv) => (true, [Some(y.clone()), Some(uv.clone())], 2),
            Views::Ayuv(a) => (false, [Some(a.clone()), None], 1),
        };
        let (ps, first) = if nv12 { (&self.ps_nv12, 0) } else { (&self.ps_ayuv, 2) };
        unsafe {
            let c = &gpu.ctx;
            c.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.RSSetState(&self.rast);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: dest.left as f32,
                TopLeftY: dest.top as f32,
                Width: dw,
                Height: dh,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            c.OMSetBlendState(None, None, u32::MAX);
            c.OMSetDepthStencilState(None, 0);
            c.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            c.PSSetShader(ps, None);
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(self.consts.clone())]));
            c.PSSetShaderResources(first, Some(&srvs[..count]));
            c.Draw(3, 0);
            // Let go of the picture, so the decoder may write it again.
            c.PSSetShaderResources(first, Some(&NONE_SRVS[..count]));
            c.OMSetRenderTargets(Some(&[None]), None);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sampling_follows_the_scale() {
        assert_eq!(super::mode(1.0), 0.0, "1:1 is exact");
        assert_eq!(super::mode(1.5), 1.0, "enlarging");
        assert_eq!(super::mode(0.75), 2.0, "shrinking a little");
        assert_eq!(super::mode(0.3), 3.0, "shrinking a lot");
    }
}
