//! Host, 4:4:4 only: the desktop's BGRA into the AYUV picture the encoder
//! takes, scaled, with a compute shader. The D3D11 video processor (which
//! makes the NV12 picture for 4:2:0) cannot write AYUV on NVIDIA's driver
//! (CreateVideoProcessorOutputView fails, 2026-11-19). Same colours as that
//! path: BT.709, limited range.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::Graphics::{
        Direct3D::{Fxc::D3DCompile, ID3DBlob},
        Direct3D11::{
            D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BIND_CONSTANT_BUFFER, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE, D3D11_FEATURE_DATA_FORMAT_SUPPORT2, D3D11_FEATURE_FORMAT_SUPPORT2,
            D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_UAV_DIMENSION_TEXTURE2D,
            D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_TEX2D_UAV, ID3D11Buffer,
            ID3D11ComputeShader, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
            ID3D11UnorderedAccessView,
        },
        Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
    },
    core::{PCSTR, s},
};

use crate::gpu::Gpu;

const SHADER: &str = r#"
Texture2D<float4> src : register(t0);
SamplerState smp : register(s0);
RWTexture2D<unorm float4> dst : register(u0);
cbuffer size : register(b0) { float2 inv_out; float2 pad; };

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint w, h;
    dst.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;
    float3 c = src.SampleLevel(smp, (float2(id.xy) + 0.5) * inv_out, 0).rgb;
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    float u = (c.b - y) / 1.8556;
    float v = (c.r - y) / 1.5748;
    // AYUV in memory is V, U, Y, A: the view's R, G, B, A.
    dst[id.xy] = float4(v * 224.0 / 255.0 + 128.0 / 255.0, u * 224.0 / 255.0 + 128.0 / 255.0, (y * 219.0 + 16.0) / 255.0, 1.0);
}
"#;

pub struct ToAyuv {
    cs: ID3D11ComputeShader,
    sampler: ID3D11SamplerState,
    consts: ID3D11Buffer,
    srv: ID3D11ShaderResourceView,
    uav: ID3D11UnorderedAccessView,
    out: (u32, u32),
}

/// Whether this graphics card can write the AYUV picture from a shader.
pub fn can_write(gpu: &Gpu) -> bool {
    let mut fs = D3D11_FEATURE_DATA_FORMAT_SUPPORT2 { InFormat: DXGI_FORMAT_R8G8B8A8_UNORM, OutFormatSupport2: 0 };
    let ok = unsafe {
        gpu.device.CheckFeatureSupport(D3D11_FEATURE_FORMAT_SUPPORT2, &mut fs as *mut _ as *mut _, size_of_val(&fs) as u32)
    };
    ok.is_ok() && fs.OutFormatSupport2 & D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE.0 as u32 != 0
}

impl ToAyuv {
    /// Converts `src` (BGRA, as captured) into `dst` (AYUV, made with
    /// `pipeline::yuv_texture`), scaled to `dst`'s size.
    pub fn new(gpu: &Gpu, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, out: (u32, u32)) -> Result<ToAyuv> {
        if !can_write(gpu) {
            bail!("this graphics card cannot write the 4:4:4 picture");
        }
        unsafe {
            let mut code: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            let r = D3DCompile(
                SHADER.as_ptr() as *const _,
                SHADER.len(),
                PCSTR::null(),
                None,
                None,
                s!("main"),
                s!("cs_5_0"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            );
            if let Err(e) = r {
                let msg = errors.map(|b| {
                    let p = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                    String::from_utf8_lossy(p).into_owned()
                });
                bail!("4:4:4 shader: {e} {}", msg.unwrap_or_default());
            }
            let code = code.context("no shader code")?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize());
            let mut cs = None;
            gpu.device.CreateComputeShader(bytes, None, Some(&mut cs))?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            gpu.device.CreateSamplerState(&sd, Some(&mut sampler))?;
            let inv = [1.0 / out.0 as f32, 1.0 / out.1 as f32, 0.0, 0.0];
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: inv.as_ptr() as *const _, ..Default::default() };
            let mut consts = None;
            gpu.device.CreateBuffer(&bd, Some(&init), Some(&mut consts))?;
            let mut srv = None;
            gpu.device.CreateShaderResourceView(src, None, Some(&mut srv)).context("4:4:4 source view")?;
            let ud = D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 } },
            };
            let mut uav = None;
            gpu.device.CreateUnorderedAccessView(dst, Some(&ud), Some(&mut uav)).context("4:4:4 picture view")?;
            Ok(ToAyuv {
                cs: cs.context("no shader")?,
                sampler: sampler.context("no sampler")?,
                consts: consts.context("no constants")?,
                srv: srv.context("no source view")?,
                uav: uav.context("no picture view")?,
                out,
            })
        }
    }

    pub fn run(&self, gpu: &Gpu) {
        unsafe {
            let c = &gpu.ctx;
            c.CSSetShader(&self.cs, None);
            c.CSSetShaderResources(0, Some(&[Some(self.srv.clone())]));
            c.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.CSSetConstantBuffers(0, Some(&[Some(self.consts.clone())]));
            c.CSSetUnorderedAccessViews(0, 1, Some(&Some(self.uav.clone())), None);
            c.Dispatch(self.out.0.div_ceil(16), self.out.1.div_ceil(16), 1);
            // Unbound, so the encoder may read the picture.
            c.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
            c.CSSetShaderResources(0, Some(&[None]));
        }
    }
}

/// Bind flags the AYUV picture needs for `ToAyuv`.
pub const AYUV_BIND: u32 = D3D11_BIND_UNORDERED_ACCESS.0 as u32;
