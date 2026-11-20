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
            D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RASTERIZER_DESC,
            D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
            D3D11_SAMPLER_DESC, D3D11_TEX2D_RTV, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_VIEWPORT, ID3D11PixelShader,
            ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
            ID3D11Texture2D, ID3D11VertexShader,
        },
        Dxgi::Common::{DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM},
    },
    core::{PCSTR, s},
};

use crate::gpu::Gpu;

const HLSL: &str = r#"
Texture2D<float4> src : register(t0);
SamplerState smp : register(s0);

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

float ps_y(VsOut i) : SV_Target {
    float3 c = src.Sample(smp, i.uv).rgb;
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return (y * 219.0 + 16.0) / 255.0;
}

float2 ps_uv(VsOut i) : SV_Target {
    float3 c = src.Sample(smp, i.uv).rgb;
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    float u = (c.b - y) / 1.8556;
    float v = (c.r - y) / 1.5748;
    return float2((u * 224.0 + 128.0) / 255.0, (v * 224.0 + 128.0) / 255.0);
}
"#;

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
}

pub struct ToNv12 {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rast: ID3D11RasterizerState,
    vp_y: D3D11_VIEWPORT,
    vp_uv: D3D11_VIEWPORT,
}

impl ToNv12 {
    pub fn new(gpu: &Gpu, out: (u32, u32)) -> Result<ToNv12> {
        unsafe {
            let vs_blob = compile(s!("vs_main"), s!("vs_5_0"))?;
            let py_blob = compile(s!("ps_y"), s!("ps_5_0"))?;
            let puv_blob = compile(s!("ps_uv"), s!("ps_5_0"))?;
            let vs_bytes = std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
            let py_bytes = std::slice::from_raw_parts(py_blob.GetBufferPointer() as *const u8, py_blob.GetBufferSize());
            let puv_bytes =
                std::slice::from_raw_parts(puv_blob.GetBufferPointer() as *const u8, puv_blob.GetBufferSize());

            let mut vs = None;
            gpu.device.CreateVertexShader(vs_bytes, None, Some(&mut vs)).context("CreateVertexShader")?;
            let mut ps_y = None;
            gpu.device.CreatePixelShader(py_bytes, None, Some(&mut ps_y)).context("CreatePixelShader(Y)")?;
            let mut ps_uv = None;
            gpu.device.CreatePixelShader(puv_bytes, None, Some(&mut ps_uv)).context("CreatePixelShader(UV)")?;

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

            Ok(ToNv12 {
                vs: vs.context("no vs")?,
                ps_y: ps_y.context("no ps_y")?,
                ps_uv: ps_uv.context("no ps_uv")?,
                sampler: sampler.context("no sampler")?,
                rast: rast.context("no rast")?,
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
            Ok(Nv12Views { srv: srv.context("no srv")?, rtv_y: rtv_y.context("no rtv_y")?, rtv_uv: rtv_uv.context("no rtv_uv")? })
        }
    }

    pub fn run(&self, gpu: &Gpu, views: &Nv12Views) {
        unsafe {
            let c = &gpu.ctx;
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.RSSetState(&self.rast);
            c.OMSetBlendState(None, None, u32::MAX);
            c.OMSetDepthStencilState(None, 0);
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.PSSetShaderResources(0, Some(&[Some(views.srv.clone())]));

            // Pass 1: Y plane
            c.RSSetViewports(Some(&[self.vp_y]));
            c.OMSetRenderTargets(Some(&[Some(views.rtv_y.clone())]), None);
            c.PSSetShader(&self.ps_y, None);
            c.Draw(3, 0);

            // Pass 2: UV plane
            c.RSSetViewports(Some(&[self.vp_uv]));
            c.OMSetRenderTargets(Some(&[Some(views.rtv_uv.clone())]), None);
            c.PSSetShader(&self.ps_uv, None);
            c.Draw(3, 0);

            // Unbind RTVs and SRV so the encoder can read dst_nv12 cleanly.
            c.OMSetRenderTargets(Some(&[None]), None);
            c.PSSetShaderResources(0, Some(&[None]));
        }
    }
}
