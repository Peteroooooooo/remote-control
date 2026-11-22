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
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC,
            D3D11_FEATURE_DATA_FORMAT_SUPPORT2, D3D11_FEATURE_FORMAT_SUPPORT2, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_UAV,
            D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_TEXTURE2D,
            D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_IMMUTABLE,
            ID3D11Buffer, ID3D11ComputeShader, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
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
cbuffer size : register(b0) { float2 inv_out; float2 step; };

float4 to_ayuv(float3 c) {
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    float u = (c.b - y) / 1.8556;
    float v = (c.r - y) / 1.5748;
    // AYUV in memory is V, U, Y, A: the view's R, G, B, A.
    return float4(v * 224.0 / 255.0 + 128.0 / 255.0, u * 224.0 / 255.0 + 128.0 / 255.0, (y * 219.0 + 16.0) / 255.0, 1.0);
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint w, h;
    dst.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;
    float3 c = src.SampleLevel(smp, (float2(id.xy) + 0.5) * inv_out, 0).rgb;
    dst[id.xy] = to_ayuv(c);
}

[numthreads(16, 16, 1)]
void main_down(uint3 id : SV_DispatchThreadID) {
    uint w, h;
    dst.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;
    float2 uv = (float2(id.xy) + 0.5) * inv_out;
    float3 c = 0.25 * (
        src.SampleLevel(smp, uv + float2(-step.x, -step.y), 0).rgb +
        src.SampleLevel(smp, uv + float2( step.x, -step.y), 0).rgb +
        src.SampleLevel(smp, uv + float2(-step.x,  step.y), 0).rgb +
        src.SampleLevel(smp, uv + float2( step.x,  step.y), 0).rgb
    );
    dst[id.xy] = to_ayuv(c);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Consts {
    inv_out: [f32; 2],
    step: [f32; 2],
}

const _: () = assert!(std::mem::size_of::<Consts>() == 16);

fn compile(entry: PCSTR) -> Result<ID3DBlob> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        if let Err(e) = D3DCompile(
            SHADER.as_ptr() as *const _,
            SHADER.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            s!("cs_5_0"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        ) {
            let msg = errors.map(|b| {
                let p = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                String::from_utf8_lossy(p).into_owned()
            });
            bail!("4:4:4 shader: {e} {}", msg.unwrap_or_default());
        }
        code.context("no shader code")
    }
}

pub struct AyuvViews {
    srv: ID3D11ShaderResourceView,
    uav: ID3D11UnorderedAccessView,
    downscale: bool,
}

pub struct ToAyuv {
    cs: ID3D11ComputeShader,
    cs_down: ID3D11ComputeShader,
    sampler: ID3D11SamplerState,
    consts: ID3D11Buffer,
    out: (u32, u32),
    default_views: Option<AyuvViews>,
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
    /// Compiles the 4:4:4 conversion shaders and shared GPU state for `out` size.
    /// Call `make_views` for each source/destination texture pair (e.g. a ring).
    pub fn shader(gpu: &Gpu, out: (u32, u32)) -> Result<ToAyuv> {
        if !can_write(gpu) {
            bail!("this graphics card cannot write the 4:4:4 picture");
        }
        unsafe {
            let code = compile(s!("main"))?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize());
            let mut cs = None;
            gpu.device.CreateComputeShader(bytes, None, Some(&mut cs))?;

            let code_down = compile(s!("main_down"))?;
            let bytes_down =
                std::slice::from_raw_parts(code_down.GetBufferPointer() as *const u8, code_down.GetBufferSize());
            let mut cs_down = None;
            gpu.device.CreateComputeShader(bytes_down, None, Some(&mut cs_down))?;

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
            let init_consts = Consts {
                inv_out: [1.0 / out.0.max(1) as f32, 1.0 / out.1.max(1) as f32],
                step: [0.25 / out.0.max(1) as f32, 0.25 / out.1.max(1) as f32],
            };
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: size_of::<Consts>() as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: &init_consts as *const _ as *const _, ..Default::default() };
            let mut consts = None;
            gpu.device.CreateBuffer(&bd, Some(&init), Some(&mut consts))?;
            Ok(ToAyuv {
                cs: cs.context("no shader")?,
                cs_down: cs_down.context("no downscale shader")?,
                sampler: sampler.context("no sampler")?,
                consts: consts.context("no constants")?,
                out,
                default_views: None,
            })
        }
    }

    /// Converts `src` (BGRA, as captured) into `dst` (AYUV, made with
    /// `pipeline::yuv_texture`), scaled to `out`.
    pub fn new(gpu: &Gpu, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, out: (u32, u32)) -> Result<ToAyuv> {
        let mut s = Self::shader(gpu, out)?;
        s.default_views = Some(s.make_views(gpu, src, dst)?);
        Ok(s)
    }

    pub fn make_views(&self, gpu: &Gpu, src: &ID3D11Texture2D, dst: &ID3D11Texture2D) -> Result<AyuvViews> {
        unsafe {
            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut src_desc);
            let downscale = src_desc.Width > self.out.0 || src_desc.Height > self.out.1;
            let mut srv = None;
            gpu.device.CreateShaderResourceView(src, None, Some(&mut srv)).context("4:4:4 source view")?;
            self.make_views_from_srv(gpu, srv.context("no source view")?, downscale, dst)
        }
    }

    pub fn make_views_from_srv(
        &self,
        gpu: &Gpu,
        srv: ID3D11ShaderResourceView,
        downscale: bool,
        dst: &ID3D11Texture2D,
    ) -> Result<AyuvViews> {
        unsafe {
            let ud = D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 } },
            };
            let mut uav = None;
            gpu.device.CreateUnorderedAccessView(dst, Some(&ud), Some(&mut uav)).context("4:4:4 picture view")?;
            Ok(AyuvViews { srv, uav: uav.context("no picture view")?, downscale })
        }
    }

    pub fn run(&self, gpu: &Gpu) {
        let views = self.default_views.as_ref().expect("ToAyuv::run requires default views");
        self.run_views(gpu, views);
    }

    pub fn run_views(&self, gpu: &Gpu, views: &AyuvViews) {
        self.run_srv(gpu, &views.srv, &views.uav, views.downscale);
    }

    pub fn run_srv(&self, gpu: &Gpu, src_srv: &ID3D11ShaderResourceView, uav: &ID3D11UnorderedAccessView, downscale: bool) {
        unsafe {
            let c = &gpu.ctx;
            let cs = if downscale { &self.cs_down } else { &self.cs };
            c.CSSetShader(cs, None);
            c.CSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
            c.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.CSSetConstantBuffers(0, Some(&[Some(self.consts.clone())]));
            c.CSSetUnorderedAccessViews(0, 1, Some(&Some(uav.clone())), None);
            c.Dispatch(self.out.0.div_ceil(16), self.out.1.div_ceil(16), 1);
            // Unbound, so the encoder may read the picture.
            c.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
            c.CSSetShaderResources(0, Some(&[None]));
        }
    }
}

/// Bind flags the AYUV picture needs for `ToAyuv`.
pub const AYUV_BIND: u32 = D3D11_BIND_UNORDERED_ACCESS.0 as u32;
