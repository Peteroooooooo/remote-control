//! Host BGRA -> NV12 converter using a D3D11 pixel shader on the 3D engine
//! (the same approach Sunshine uses).
//!
//! D3D11 `VideoProcessorBlt` routes through the fixed-function VEBOX/VPP block
//! (`engtype_videoprocessing`), which on Intel Arc adds ~4–6 ms per frame at
//! high resolution and leaves the 3D engine idle so media clocks stay low.
//! Two lightweight fullscreen-triangle passes (R8 Y plane + R8G8 UV plane) run
//! in sub-millisecond time on any D3D11 GPU while freeing VEBOX completely.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::Graphics::{
        Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob},
        Direct3D11::{
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CULL_NONE, D3D11_FILL_SOLID,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_VIEW_DESC,
            D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC,
            D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11Buffer, ID3D11PixelShader, ID3D11RasterizerState,
            ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
            ID3D11VertexShader,
        },
        Dxgi::Common::{DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT},
    },
    core::{PCSTR, s},
};

use crate::gpu::Gpu;

const HLSL: &str = r#"
Texture2D<float4> src : register(t0);
SamplerState smp : register(s0);

cbuffer consts : register(b0) {
    float uv_shift_x;
    float step_x;
    float step_y;
    float inv_sdr_white;
};

struct VsOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float3 hdr_to_srgb(float3 c) {
    c = max(c, 0.0) * inv_sdr_white;
    float l = dot(c, float3(0.2126, 0.7152, 0.0722));
    if (l > 0.85) {
        float over = l - 0.85;
        float l_new = 0.85 + 0.15 * (over / (over + 0.15));
        c *= (l_new / l);
    }
    c = saturate(c);
    float3 lo = c * 12.92;
    float3 hi = 1.055 * pow(max(c, 0.0031308), 1.0 / 2.4) - 0.055;
    return lerp(lo, hi, step(0.0031308, c));
}

float ps_y(VsOut i) : SV_Target {
    float3 c = src.Sample(smp, i.uv).rgb;
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return (y * 219.0 + 16.0) / 255.0;
}

// ---- 1:1 chroma with ClearType de-fringe -------------------------------------------------
// ClearType paints thin opposite-hue edges (red on one side of a stem, blue on the other).
// 4:2:0 keeps one chroma sample per 2x2 block, so those edges survive as red/blue bars on
// text. Before downsampling chroma we look along each row: where a pixel sits within 2 px of
// an opposite-direction chroma pair, its chroma is replaced by a 1-4-6-4-1 average, which
// cancels the fringe. Real colored text has same-direction chroma and is left alone. Luma is
// computed by ps_y from the untouched pixels, so sharpness is unchanged.
static const float3 LUMA_W = float3(0.2126, 0.7152, 0.0722);
static const float DEFRINGE_T2 = (6.0 / 255.0) * (6.0 / 255.0);

float3 load_px(int2 p, int2 dims, bool hdr) {
    p = clamp(p, int2(0, 0), dims - 1);
    float3 c = src.Load(int3(p, 0)).rgb;
    return hdr ? hdr_to_srgb(c) : c;
}

float3 defringe_at(float3 ch[7], int j) {
    bool strong = false;
    bool opposite = false;
    [unroll] for (int a = -2; a <= 2; a++) {
        float3 ca = ch[j + a];
        if (dot(ca, ca) > DEFRINGE_T2) strong = true;
        [unroll] for (int b = a + 1; b <= 2; b++) {
            if (dot(ca, ch[j + b]) < -DEFRINGE_T2) opposite = true;
        }
    }
    if (!(strong && opposite)) return ch[j];
    return (ch[j - 2] + 4.0 * ch[j - 1] + 6.0 * ch[j] + 4.0 * ch[j + 1] + ch[j + 2]) / 16.0;
}

// Chroma (rgb - luma) for one source row at column x0, left-sited [1 2 1] downsample.
float3 row_chroma(int x0, int y, int2 dims, bool hdr) {
    float3 ch[7];
    [unroll] for (int k = 0; k < 7; k++) {
        float3 c = load_px(int2(x0 - 3 + k, y), dims, hdr);
        ch[k] = c - dot(c, LUMA_W);
    }
    return 0.25 * defringe_at(ch, 2) + 0.5 * defringe_at(ch, 3) + 0.25 * defringe_at(ch, 4);
}

float2 uv_defringed(VsOut i, bool hdr) {
    uint w, h;
    src.GetDimensions(w, h);
    int2 dims = int2(w, h);
    // Top-left source pixel of this chroma sample's 2x2 block.
    int2 base = int2(floor(i.uv * float2(dims) - 0.5));
    float3 cc = 0.5 * (row_chroma(base.x, base.y, dims, hdr) + row_chroma(base.x, base.y + 1, dims, hdr));
    float u = cc.b / 1.8556;
    float v = cc.r / 1.5748;
    return float2((u * 224.0 + 128.0) / 255.0, (v * 224.0 + 128.0) / 255.0);
}

float2 ps_uv(VsOut i) : SV_Target {
    return uv_defringed(i, false);
}

float ps_y_down(VsOut i) : SV_Target {
    float3 c = 0.25 * (
        src.Sample(smp, i.uv + float2(-step_x, -step_y)).rgb +
        src.Sample(smp, i.uv + float2( step_x, -step_y)).rgb +
        src.Sample(smp, i.uv + float2(-step_x,  step_y)).rgb +
        src.Sample(smp, i.uv + float2( step_x,  step_y)).rgb
    );
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return (y * 219.0 + 16.0) / 255.0;
}

float2 ps_uv_down(VsOut i) : SV_Target {
    float2 base = i.uv + float2(uv_shift_x, 0.0);
    float3 c = 0.25 * (
        src.Sample(smp, base + float2(-step_x, -step_y)).rgb +
        src.Sample(smp, base + float2( step_x, -step_y)).rgb +
        src.Sample(smp, base + float2(-step_x,  step_y)).rgb +
        src.Sample(smp, base + float2( step_x,  step_y)).rgb
    );
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    float u = (c.b - y) / 1.8556;
    float v = (c.r - y) / 1.5748;
    return float2((u * 224.0 + 128.0) / 255.0, (v * 224.0 + 128.0) / 255.0);
}

float ps_y_hdr(VsOut i) : SV_Target {
    float3 c = hdr_to_srgb(src.Sample(smp, i.uv).rgb);
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return (y * 219.0 + 16.0) / 255.0;
}

float2 ps_uv_hdr(VsOut i) : SV_Target {
    return uv_defringed(i, true);
}

float ps_y_down_hdr(VsOut i) : SV_Target {
    float3 c = hdr_to_srgb(0.25 * (
        src.Sample(smp, i.uv + float2(-step_x, -step_y)).rgb +
        src.Sample(smp, i.uv + float2( step_x, -step_y)).rgb +
        src.Sample(smp, i.uv + float2(-step_x,  step_y)).rgb +
        src.Sample(smp, i.uv + float2( step_x,  step_y)).rgb
    ));
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return (y * 219.0 + 16.0) / 255.0;
}

float2 ps_uv_down_hdr(VsOut i) : SV_Target {
    float2 base = i.uv + float2(uv_shift_x, 0.0);
    float3 c = hdr_to_srgb(0.25 * (
        src.Sample(smp, base + float2(-step_x, -step_y)).rgb +
        src.Sample(smp, base + float2( step_x, -step_y)).rgb +
        src.Sample(smp, base + float2(-step_x,  step_y)).rgb +
        src.Sample(smp, base + float2( step_x,  step_y)).rgb
    ));
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    float u = (c.b - y) / 1.8556;
    float v = (c.r - y) / 1.5748;
    return float2((u * 224.0 + 128.0) / 255.0, (v * 224.0 + 128.0) / 255.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Consts {
    uv_shift_x: f32,
    step_x: f32,
    step_y: f32,
    inv_sdr_white: f32,
}

const _: () = assert!(std::mem::size_of::<Consts>() == 16);

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        if let Err(e) = D3DCompile(
            HLSL.as_ptr() as *const _,
            HLSL.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        ) {
            let msg = errors.map(|b| {
                let p = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                String::from_utf8_lossy(p).into_owned()
            });
            bail!("nv12 shader: {e} {}", msg.unwrap_or_default());
        }
        code.context("no shader code")
    }
}

pub struct Nv12Views {
    srv: ID3D11ShaderResourceView,
    rtv_y: ID3D11RenderTargetView,
    rtv_uv: ID3D11RenderTargetView,
    downscale: bool,
    hdr: bool,
}

pub struct ToNv12 {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    ps_y_down: ID3D11PixelShader,
    ps_uv_down: ID3D11PixelShader,
    ps_y_hdr: ID3D11PixelShader,
    ps_uv_hdr: ID3D11PixelShader,
    ps_y_down_hdr: ID3D11PixelShader,
    ps_uv_down_hdr: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rast: ID3D11RasterizerState,
    consts: ID3D11Buffer,
    out: (u32, u32),
    vp_y: D3D11_VIEWPORT,
    vp_uv: D3D11_VIEWPORT,
}

impl ToNv12 {
    pub fn new_with_hdr(gpu: &Gpu, out: (u32, u32), inv_sdr_white: f32) -> Result<ToNv12> {
        unsafe {
            let create_ps = |entry: PCSTR, label: &str| -> Result<ID3D11PixelShader> {
                let blob = compile(entry, s!("ps_5_0"))?;
                let bytes = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
                let mut ps = None;
                gpu.device
                    .CreatePixelShader(bytes, None, Some(&mut ps))
                    .with_context(|| format!("CreatePixelShader({label})"))?;
                ps.with_context(|| format!("no {label}"))
            };
            let vs_blob = compile(s!("vs_main"), s!("vs_5_0"))?;
            let vs_bytes = std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
            let mut vs = None;
            gpu.device.CreateVertexShader(vs_bytes, None, Some(&mut vs)).context("CreateVertexShader")?;

            let ps_y = create_ps(s!("ps_y"), "Y")?;
            let ps_uv = create_ps(s!("ps_uv"), "UV")?;
            let ps_y_down = create_ps(s!("ps_y_down"), "Y down")?;
            let ps_uv_down = create_ps(s!("ps_uv_down"), "UV down")?;
            let ps_y_hdr = create_ps(s!("ps_y_hdr"), "Y HDR")?;
            let ps_uv_hdr = create_ps(s!("ps_uv_hdr"), "UV HDR")?;
            let ps_y_down_hdr = create_ps(s!("ps_y_down_hdr"), "Y down HDR")?;
            let ps_uv_down_hdr = create_ps(s!("ps_uv_down_hdr"), "UV down HDR")?;

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

            let init = Consts {
                uv_shift_x: -0.5 / out.0.max(1) as f32,
                step_x: 0.25 / out.0.max(1) as f32,
                step_y: 0.25 / out.1.max(1) as f32,
                inv_sdr_white,
            };
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: size_of::<Consts>() as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let sub = D3D11_SUBRESOURCE_DATA { pSysMem: &init as *const _ as *const _, ..Default::default() };
            let mut consts = None;
            gpu.device.CreateBuffer(&bd, Some(&sub), Some(&mut consts)).context("CreateBuffer(nv12 consts)")?;

            Ok(ToNv12 {
                vs: vs.context("no vs")?,
                ps_y,
                ps_uv,
                ps_y_down,
                ps_uv_down,
                ps_y_hdr,
                ps_uv_hdr,
                ps_y_down_hdr,
                ps_uv_down_hdr,
                sampler: sampler.context("no sampler")?,
                rast: rast.context("no rast")?,
                consts: consts.context("no consts")?,
                out,
                vp_y: D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: out.0 as f32,
                    Height: out.1 as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                },
                vp_uv: D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: (out.0 / 2) as f32,
                    Height: (out.1 / 2) as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                },
            })
        }
    }

    pub fn make_views(&self, gpu: &Gpu, src: &ID3D11Texture2D, dst_nv12: &ID3D11Texture2D) -> Result<Nv12Views> {
        unsafe {
            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut src_desc);
            let downscale = src_desc.Width > self.out.0 || src_desc.Height > self.out.1;
            let hdr = src_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;

            let mut srv = None;
            gpu.device.CreateShaderResourceView(src, None, Some(&mut srv)).context("NV12 src SRV")?;
            let y_desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_R8_UNORM,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
            };
            let mut rtv_y = None;
            gpu.device.CreateRenderTargetView(dst_nv12, Some(&y_desc), Some(&mut rtv_y)).context("NV12 Y RTV")?;
            let uv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8_UNORM,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
            };
            let mut rtv_uv = None;
            gpu.device.CreateRenderTargetView(dst_nv12, Some(&uv_desc), Some(&mut rtv_uv)).context("NV12 UV RTV")?;
            let rtv_y = rtv_y.context("no rtv_y")?;
            let rtv_uv = rtv_uv.context("no rtv_uv")?;
            let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
            dst_nv12.GetDesc(&mut dst_desc);
            if dst_desc.Width > self.out.0 || dst_desc.Height > self.out.1 {
                // Pre-fill alignment padding with black (Y=0, UV=0.5) so edges stay dark instead of green.
                gpu.ctx.ClearRenderTargetView(&rtv_y, &[0.0, 0.0, 0.0, 1.0]);
                gpu.ctx.ClearRenderTargetView(&rtv_uv, &[0.5, 0.5, 0.0, 1.0]);
            }
            Ok(Nv12Views { srv: srv.context("no srv")?, rtv_y, rtv_uv, downscale, hdr })
        }
    }

    pub fn run(&self, gpu: &Gpu, views: &Nv12Views) {
        unsafe {
            let c = &gpu.ctx;
            let (ps_y, ps_uv) = match (views.hdr, views.downscale) {
                (false, false) => (&self.ps_y, &self.ps_uv),
                (false, true) => (&self.ps_y_down, &self.ps_uv_down),
                (true, false) => (&self.ps_y_hdr, &self.ps_uv_hdr),
                (true, true) => (&self.ps_y_down_hdr, &self.ps_uv_down_hdr),
            };
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.RSSetState(&self.rast);
            c.OMSetBlendState(None, None, u32::MAX);
            c.OMSetDepthStencilState(None, 0);
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(self.consts.clone())]));
            c.PSSetShaderResources(0, Some(&[Some(views.srv.clone())]));

            // Pass 1: Y plane
            c.RSSetViewports(Some(&[self.vp_y]));
            c.OMSetRenderTargets(Some(&[Some(views.rtv_y.clone())]), None);
            c.PSSetShader(ps_y, None);
            c.Draw(3, 0);

            // Pass 2: UV plane
            c.RSSetViewports(Some(&[self.vp_uv]));
            c.OMSetRenderTargets(Some(&[Some(views.rtv_uv.clone())]), None);
            c.PSSetShader(ps_uv, None);
            c.Draw(3, 0);

            // Unbind RTVs and SRV so the encoder can read dst_nv12 cleanly.
            c.OMSetRenderTargets(Some(&[None]), None);
            c.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hlsl_sdr_and_hdr_shaders_compile_on_d3d11() {
        // Verify FXC compiles every SDR and HDR shader entry cleanly.
        for (entry, target) in [
            (s!("vs_main"), s!("vs_5_0")),
            (s!("ps_y"), s!("ps_5_0")),
            (s!("ps_uv"), s!("ps_5_0")),
            (s!("ps_y_down"), s!("ps_5_0")),
            (s!("ps_uv_down"), s!("ps_5_0")),
            (s!("ps_y_hdr"), s!("ps_5_0")),
            (s!("ps_uv_hdr"), s!("ps_5_0")),
            (s!("ps_y_down_hdr"), s!("ps_5_0")),
            (s!("ps_uv_down_hdr"), s!("ps_5_0")),
        ] {
            let blob = compile(entry, target).expect("nv12 HLSL compile failed");
            assert!(unsafe { blob.GetBufferSize() } > 0);
        }
    }

    #[test]
    fn fp16_hdr_views_and_gpu_pass_execute_cleanly() {
        let Ok(gpu) = Gpu::default_adapter() else { return };
        let conv = ToNv12::new_with_hdr(&gpu, (64, 64), 1.0 / 2.5).expect("ToNv12::new_with_hdr");
        let src = crate::capture::fp16_texture(&gpu, 64, 64, 0).expect("fp16_texture");
        let dst = crate::pipeline::nv12_texture(&gpu, 64, 64).expect("nv12_texture");
        let views = conv.make_views(&gpu, &src, &dst).expect("make_views for FP16");
        assert!(views.hdr, "FP16 source texture must activate HDR shader path");
        assert!(!views.downscale);
        conv.run(&gpu, &views);

        // SDR BGRA source must stay on zero-overhead non-HDR path.
        let src_sdr = crate::capture::bgra_texture(&gpu, 64, 64, 0).expect("bgra_texture");
        let views_sdr = conv.make_views(&gpu, &src_sdr, &dst).expect("make_views for BGRA");
        assert!(!views_sdr.hdr, "BGRA8 source texture must stay on SDR shader path");
        conv.run(&gpu, &views_sdr);
    }
}
