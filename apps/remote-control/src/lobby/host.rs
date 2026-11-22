//! Lobby window host: `winit` + `egui-winit` + a minimal D3D11 egui renderer.
//!
//! Why not `eframe` / `wgpu` (DX12 flip model)?
//! Flip-model swap chains (`DXGI_SWAP_EFFECT_FLIP_*`) present asynchronously
//! relative to DWM window frame geometry changes, so rapidly dragging the top
//! edge exposes white/black gaps behind the window, and `wgpu-hal` hardcodes
//! `DXGI_SCALING_STRETCH` on HWND swap chains, stretching the stale backbuffer
//! horizontally while dragging left/right.
//!
//! A classic D3D11 swap chain with `DXGI_SWAP_EFFECT_SEQUENTIAL` copies to the
//! window's DWM redirection surface, which DWM synchronizes atomically with
//! window border updates when resized and presented synchronously inside
//! `WM_SIZE` (`WindowEvent::Resized`).

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use egui::{Color32, TextureFilter, TextureId, TextureOptions, TextureWrapMode, ViewportId, epaint};
use egui_winit::{
    accesskit_winit,
    winit::{
        application::ApplicationHandler,
        dpi::LogicalSize,
        event::{DeviceEvent, DeviceId, WindowEvent},
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
        window::{Theme, Window, WindowId},
    },
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
                D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile,
                ID3DBlob,
            },
            Direct3D11::{
                D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_INDEX_BUFFER, D3D11_BIND_SHADER_RESOURCE,
                D3D11_BIND_VERTEX_BUFFER, D3D11_BLEND_DESC, D3D11_BLEND_INV_DEST_ALPHA, D3D11_BLEND_INV_SRC_ALPHA,
                D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
                D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CULL_NONE, D3D11_FILL_SOLID,
                D3D11_FILTER_MIN_LINEAR_MAG_POINT_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_FILTER_MIN_POINT_MAG_LINEAR_MIP_POINT,
                D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA, D3D11_MAP_WRITE_DISCARD,
                D3D11_MAPPED_SUBRESOURCE, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC,
                D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE_ADDRESS_MIRROR,
                D3D11_TEXTURE_ADDRESS_WRAP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
                D3D11_VIEWPORT, D3D11CreateDevice, ID3D11BlendState, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
                ID3D11InputLayout, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView,
                ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
            },
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
                },
                DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
                DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_SEQUENTIAL,
                DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1,
            },
            Gdi::{CreateSolidBrush, DeleteObject, HBRUSH},
        },
        UI::WindowsAndMessaging::{
            CallWindowProcW, GCLP_HBRBACKGROUND, GWLP_WNDPROC, GetWindowLongPtrW, IsIconic, IsWindowVisible,
            SetClassLongPtrW, SetWindowLongPtrW, WM_ERASEBKGND, WNDPROC,
        },
    },
    core::{Interface, PCSTR, s},
};

use crate::theme;

const _: () = assert!(size_of::<epaint::Vertex>() == 20);
const _: () = assert!(size_of::<Color32>() == 4);

pub trait LobbyApp {
    fn clear_color(&self) -> [f32; 4];
    fn ui(&mut self, ui: &mut egui::Ui);
    fn on_close(&mut self, ctx: &egui::Context);
    fn on_exit(&mut self);
}

pub struct Config {
    pub title: &'static str,
    pub inner_size: [f64; 2],
    pub min_inner_size: [f64; 2],
    pub hidden: bool,
}

enum UserEvent {
    RequestRepaint { when: Instant },
    AccessKit(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(e: accesskit_winit::Event) -> Self {
        Self::AccessKit(e)
    }
}

static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);
static CHROME_BRUSH: AtomicIsize = AtomicIsize::new(0);

fn chrome_brush() -> HBRUSH {
    let cur = CHROME_BRUSH.load(Ordering::Relaxed);
    if cur != 0 {
        return HBRUSH(cur as *mut _);
    }
    let created = unsafe { CreateSolidBrush(crate::gdi::rgb(theme::CHROME)) };
    let val = created.0 as isize;
    match CHROME_BRUSH.compare_exchange(0, val, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => created,
        Err(existing) => {
            unsafe {
                let _ = DeleteObject(created.into());
            }
            HBRUSH(existing as *mut _)
        }
    }
}

unsafe extern "system" fn no_erase_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    let orig = ORIG_WNDPROC.load(Ordering::Acquire);
    let proc_fn: WNDPROC = unsafe { std::mem::transmute(orig) };
    unsafe { CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam) }
}

fn install_no_erase(hwnd: HWND) {
    let brush = chrome_brush();
    if !brush.is_invalid() {
        unsafe {
            SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush.0 as isize);
        }
    }
    let ours = no_erase_wndproc as *const () as isize;
    let current = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    if current != 0 && current != ours {
        ORIG_WNDPROC.store(current, Ordering::Release);
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ours);
        }
    }
}

// ---------------------------------------------------------------------------
// D3D11 egui renderer (gamma-space B8G8R8A8_UNORM + DXGI_SWAP_EFFECT_SEQUENTIAL)
// ---------------------------------------------------------------------------

const HLSL: &str = r#"
cbuffer Locals : register(b0) {
    float2 screen_size_in_points;
    float2 _pad;
};

struct VsIn {
    float2 pos : POSITION;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};

struct VsOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};

Texture2D t_tex : register(t0);
SamplerState s_smp : register(s0);

VsOut vs_main(VsIn i) {
    VsOut o;
    o.pos = float4(
        2.0 * i.pos.x / screen_size_in_points.x - 1.0,
        1.0 - 2.0 * i.pos.y / screen_size_in_points.y,
        0.0,
        1.0
    );
    o.uv = i.uv;
    o.col = i.col;
    return o;
}

float4 ps_main(VsOut i) : SV_Target {
    float4 tex = t_tex.Sample(s_smp, i.uv);
    float4 col = i.col * tex;
    // Interleaved gradient noise dithering matching egui-wgpu to prevent
    // 8-bit banding on subtle gradients and shadows.
    float noise = frac(52.9829189 * frac(0.06711056 * i.pos.x + 0.00583715 * i.pos.y));
    float dither = (noise - 0.5) * (0.95 / 255.0);
    col.rgb = clamp(col.rgb + dither * col.a, 0.0, col.a);
    return col;
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Locals {
    screen_size_in_points: [f32; 2],
    _pad: [f32; 2],
}

struct GpuTexture {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    options: TextureOptions,
    /// CPU-side RGBA8 backup so GPU driver reset / device-lost can recreate
    /// the font atlas and any UI textures without losing state.
    pixels: Vec<u8>,
}

struct GpuCore {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    blend: ID3D11BlendState,
    rast: ID3D11RasterizerState,
    cbuf: ID3D11Buffer,
    vb: Option<(ID3D11Buffer, usize)>,
    ib: Option<(ID3D11Buffer, usize)>,
    samplers: HashMap<(u8, u8, u8), ID3D11SamplerState>,
    max_texture_side: usize,
}

struct Renderer {
    hwnd: HWND,
    size: (u32, u32),
    gpu: GpuCore,
    textures: HashMap<TextureId, GpuTexture>,
    scratch_verts: Vec<epaint::Vertex>,
    scratch_indices: Vec<u32>,
}

fn compile_hlsl(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
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
            bail!("egui shader compile failed: {e} {}", msg.unwrap_or_default());
        }
        code.context("no compiled shader bytecode")
    }
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, D3D_FEATURE_LEVEL)> {
    let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    let mut last_err = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut dev = None;
        let mut ctx = None;
        let mut level = D3D_FEATURE_LEVEL(0);
        let res = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                Some(&mut level),
                Some(&mut ctx),
            )
        };
        match res {
            Ok(()) => {
                if let (Some(d), Some(c)) = (dev, ctx) {
                    return Ok((d, c, level));
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!("D3D11CreateDevice failed: {:?}", last_err))
}

fn create_rtv(device: &ID3D11Device, swap: &IDXGISwapChain1) -> Result<ID3D11RenderTargetView> {
    unsafe {
        let back: ID3D11Texture2D = swap.GetBuffer(0).context("GetBuffer(0)")?;
        let mut rtv = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv)).context("CreateRenderTargetView")?;
        rtv.context("null RTV")
    }
}

fn create_gpu_core(hwnd: HWND, size: (u32, u32)) -> Result<GpuCore> {
    let (device, ctx, level) = create_device()?;
    let max_texture_side = if level.0 >= D3D_FEATURE_LEVEL_11_0.0 { 16384 } else { 8192 };

    let swap = unsafe {
        let dxgi_dev: IDXGIDevice = device.cast().context("IDXGIDevice")?;
        let adapter = dxgi_dev.GetAdapter().context("GetAdapter")?;
        let factory: IDXGIFactory2 = adapter.GetParent().context("IDXGIFactory2")?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0.max(1),
            Height: size.1.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: 0,
            ..Default::default()
        };
        let swap = factory
            .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
            .context("CreateSwapChainForHwnd")?;
        let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
        swap
    };

    let rtv = create_rtv(&device, &swap)?;

    let vs_blob = compile_hlsl(s!("vs_main"), s!("vs_4_0"))?;
    let ps_blob = compile_hlsl(s!("ps_main"), s!("ps_4_0"))?;

    let (vs, ps, input_layout, blend, rast, cbuf) = unsafe {
        let mut vs = None;
        device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs)).context("CreateVertexShader")?;
        let mut ps = None;
        device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps)).context("CreatePixelShader")?;

        let elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("POSITION"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("COLOR"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                InputSlot: 0,
                AlignedByteOffset: 16,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let mut input_layout = None;
        device
            .CreateInputLayout(&elements, blob_bytes(&vs_blob), Some(&mut input_layout))
            .context("CreateInputLayout")?;

        let mut blend_desc = D3D11_BLEND_DESC::default();
        blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_INV_DEST_ALPHA,
            DestBlendAlpha: D3D11_BLEND_ONE,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut blend = None;
        device.CreateBlendState(&blend_desc, Some(&mut blend)).context("CreateBlendState")?;

        let rast_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: true.into(),
            DepthClipEnable: false.into(),
            ..Default::default()
        };
        let mut rast = None;
        device.CreateRasterizerState(&rast_desc, Some(&mut rast)).context("CreateRasterizerState")?;

        let cbuf_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<Locals>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cbuf = None;
        device.CreateBuffer(&cbuf_desc, None, Some(&mut cbuf)).context("CreateBuffer(cbuf)")?;

        (
            vs.context("null VS")?,
            ps.context("null PS")?,
            input_layout.context("null input layout")?,
            blend.context("null blend")?,
            rast.context("null rast")?,
            cbuf.context("null cbuf")?,
        )
    };

    Ok(GpuCore {
        device,
        ctx,
        swap,
        rtv: Some(rtv),
        vs,
        ps,
        input_layout,
        blend,
        rast,
        cbuf,
        vb: None,
        ib: None,
        samplers: HashMap::new(),
        max_texture_side,
    })
}

fn create_texture_and_srv(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr() as *const _,
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    unsafe {
        let mut tex = None;
        device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).context("CreateTexture2D")?;
        let tex = tex.context("null texture")?;
        let mut srv = None;
        device.CreateShaderResourceView(&tex, None, Some(&mut srv)).context("CreateShaderResourceView")?;
        let srv = srv.context("null SRV")?;
        Ok((tex, srv))
    }
}

fn sampler_key(opts: TextureOptions) -> (u8, u8, u8) {
    let f = |t: TextureFilter| match t {
        TextureFilter::Nearest => 0,
        TextureFilter::Linear => 1,
    };
    let w = match opts.wrap_mode {
        TextureWrapMode::ClampToEdge => 0,
        TextureWrapMode::Repeat => 1,
        TextureWrapMode::MirroredRepeat => 2,
    };
    (f(opts.magnification), f(opts.minification), w)
}

fn get_or_create_sampler<'a>(
    device: &ID3D11Device,
    cache: &'a mut HashMap<(u8, u8, u8), ID3D11SamplerState>,
    opts: TextureOptions,
) -> Result<&'a ID3D11SamplerState> {
    let key = sampler_key(opts);
    if !cache.contains_key(&key) {
        let filter = match (opts.magnification, opts.minification) {
            (TextureFilter::Nearest, TextureFilter::Nearest) => D3D11_FILTER_MIN_MAG_MIP_POINT,
            (TextureFilter::Linear, TextureFilter::Linear) => D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            (TextureFilter::Linear, TextureFilter::Nearest) => D3D11_FILTER_MIN_POINT_MAG_LINEAR_MIP_POINT,
            (TextureFilter::Nearest, TextureFilter::Linear) => D3D11_FILTER_MIN_LINEAR_MAG_POINT_MIP_LINEAR,
        };
        let addr = match opts.wrap_mode {
            TextureWrapMode::ClampToEdge => D3D11_TEXTURE_ADDRESS_CLAMP,
            TextureWrapMode::Repeat => D3D11_TEXTURE_ADDRESS_WRAP,
            TextureWrapMode::MirroredRepeat => D3D11_TEXTURE_ADDRESS_MIRROR,
        };
        let desc = D3D11_SAMPLER_DESC {
            Filter: filter,
            AddressU: addr,
            AddressV: addr,
            AddressW: addr,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut smp = None;
        unsafe { device.CreateSamplerState(&desc, Some(&mut smp)) }.context("CreateSamplerState")?;
        cache.insert(key, smp.context("null sampler")?);
    }
    Ok(cache.get(&key).expect("inserted above"))
}

fn is_device_lost(err: windows::core::HRESULT) -> bool {
    err == DXGI_ERROR_DEVICE_REMOVED || err == DXGI_ERROR_DEVICE_RESET
}

impl Renderer {
    fn new(hwnd: HWND, size: (u32, u32)) -> Result<Self> {
        let size = (size.0.max(1), size.1.max(1));
        let gpu = create_gpu_core(hwnd, size)?;
        Ok(Self {
            hwnd,
            size,
            gpu,
            textures: HashMap::new(),
            scratch_verts: Vec::new(),
            scratch_indices: Vec::new(),
        })
    }

    fn max_texture_side(&self) -> usize {
        self.gpu.max_texture_side
    }

    fn recreate_after_device_lost(&mut self) -> Result<()> {
        let new_gpu = create_gpu_core(self.hwnd, self.size)?;
        let mut rebuilt = HashMap::with_capacity(self.textures.len());
        for (id, old) in &self.textures {
            let (tex, srv) = create_texture_and_srv(&new_gpu.device, old.width, old.height, &old.pixels)?;
            rebuilt.insert(
                *id,
                GpuTexture {
                    tex,
                    srv,
                    width: old.width,
                    height: old.height,
                    options: old.options,
                    pixels: old.pixels.clone(),
                },
            );
        }
        self.gpu = new_gpu;
        self.textures = rebuilt;
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || (width, height) == self.size {
            return Ok(());
        }
        unsafe {
            self.gpu.ctx.OMSetRenderTargets(Some(&[None]), None);
        }
        self.gpu.rtv = None;
        let res = unsafe {
            self.gpu
                .swap
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
        };
        if let Err(e) = res {
            if is_device_lost(e.code()) {
                self.size = (width, height);
                return self.recreate_after_device_lost();
            }
            return Err(e).context("ResizeBuffers");
        }
        self.size = (width, height);
        self.gpu.rtv = Some(create_rtv(&self.gpu.device, &self.gpu.swap)?);
        Ok(())
    }

    fn update_texture(&mut self, id: TextureId, delta: &epaint::ImageDelta) -> Result<()> {
        let epaint::ImageData::Color(image) = &delta.image;
        let w = image.size[0] as u32;
        let h = image.size[1] as u32;
        if w == 0 || h == 0 {
            return Ok(());
        }
        let src_bytes = unsafe { std::slice::from_raw_parts(image.pixels.as_ptr() as *const u8, image.pixels.len() * 4) };

        if let Some([x, y]) = delta.pos
            && let Some(entry) = self.textures.get_mut(&id)
        {
            let x = x as u32;
            let y = y as u32;
            if x + w <= entry.width && y + h <= entry.height {
                let dst_stride = (entry.width * 4) as usize;
                let src_stride = (w * 4) as usize;
                for row in 0..h as usize {
                    let dst_off = (y as usize + row) * dst_stride + (x as usize) * 4;
                    let src_off = row * src_stride;
                    entry.pixels[dst_off..dst_off + src_stride]
                        .copy_from_slice(&src_bytes[src_off..src_off + src_stride]);
                }
                let bx = D3D11_BOX { left: x, top: y, front: 0, right: x + w, bottom: y + h, back: 1 };
                unsafe {
                    self.gpu
                        .ctx
                        .UpdateSubresource(&entry.tex, 0, Some(&bx), src_bytes.as_ptr() as *const _, w * 4, 0);
                }
                entry.options = delta.options;
                return Ok(());
            }
        }

        let (tex, srv) = create_texture_and_srv(&self.gpu.device, w, h, src_bytes)?;
        self.textures.insert(
            id,
            GpuTexture { tex, srv, width: w, height: h, options: delta.options, pixels: src_bytes.to_vec() },
        );
        Ok(())
    }

    fn ensure_dynamic_buffer(
        device: &ID3D11Device,
        slot: &mut Option<(ID3D11Buffer, usize)>,
        needed_bytes: usize,
        bind_flag: u32,
    ) -> Result<ID3D11Buffer> {
        let cur_cap = slot.as_ref().map_or(0, |(_, cap)| *cap);
        if cur_cap < needed_bytes {
            let cap = needed_bytes.next_power_of_two().max(4096);
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: cap as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: bind_flag,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut buf = None;
            unsafe { device.CreateBuffer(&desc, None, Some(&mut buf)) }.context("CreateBuffer(dynamic)")?;
            *slot = Some((buf.context("null dynamic buffer")?, cap));
        }
        Ok(slot.as_ref().expect("created above").0.clone())
    }

    fn paint(
        &mut self,
        clear_color: [f32; 4],
        pixels_per_point: f32,
        textures_delta: &egui::TexturesDelta,
        primitives: &[egui::ClippedPrimitive],
        sync_interval: u32,
    ) -> Result<()> {
        for (id, delta) in &textures_delta.set {
            self.update_texture(*id, delta)?;
        }

        if self.gpu.rtv.is_none() {
            self.gpu.rtv = Some(create_rtv(&self.gpu.device, &self.gpu.swap)?);
        }

        let (w, h) = self.size;
        let ppp = pixels_per_point.max(0.1);

        // Flatten all meshes into a single VB + IB upload per frame.
        self.scratch_verts.clear();
        self.scratch_indices.clear();
        struct DrawCall {
            scissor: RECT,
            tex: TextureId,
            index_offset: u32,
            index_count: u32,
            vertex_offset: i32,
        }
        let mut draws = Vec::with_capacity(primitives.len());
        for prim in primitives {
            let epaint::Primitive::Mesh(mesh) = &prim.primitive else {
                continue;
            };
            if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                continue;
            }
            let r = prim.clip_rect;
            let left = (r.min.x * ppp).round() as i32;
            let top = (r.min.y * ppp).round() as i32;
            let right = (r.max.x * ppp).round() as i32;
            let bottom = (r.max.y * ppp).round() as i32;
            let left = left.clamp(0, w as i32);
            let top = top.clamp(0, h as i32);
            let right = right.clamp(left, w as i32);
            let bottom = bottom.clamp(top, h as i32);
            if right <= left || bottom <= top {
                continue;
            }
            let vertex_offset = self.scratch_verts.len() as i32;
            let index_offset = self.scratch_indices.len() as u32;
            self.scratch_verts.extend_from_slice(&mesh.vertices);
            self.scratch_indices.extend_from_slice(&mesh.indices);
            draws.push(DrawCall {
                scissor: RECT { left, top, right, bottom },
                tex: mesh.texture_id,
                index_offset,
                index_count: mesh.indices.len() as u32,
                vertex_offset,
            });
        }

        unsafe {
            let ctx = &self.gpu.ctx;
            let rtv = self.gpu.rtv.as_ref().expect("rtv ensured above");
            ctx.ClearRenderTargetView(rtv, &clear_color);

            if !draws.is_empty() {
                let locals = Locals {
                    screen_size_in_points: [w as f32 / ppp, h as f32 / ppp],
                    _pad: [0.0, 0.0],
                };
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&self.gpu.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                    .context("Map(cbuf)")?;
                std::ptr::copy_nonoverlapping(&locals, mapped.pData as *mut Locals, 1);
                ctx.Unmap(&self.gpu.cbuf, 0);

                let vb_bytes = std::mem::size_of_val(self.scratch_verts.as_slice());
                let ib_bytes = std::mem::size_of_val(self.scratch_indices.as_slice());
                let vb = Self::ensure_dynamic_buffer(
                    &self.gpu.device,
                    &mut self.gpu.vb,
                    vb_bytes,
                    D3D11_BIND_VERTEX_BUFFER.0 as u32,
                )?;
                let ib = Self::ensure_dynamic_buffer(
                    &self.gpu.device,
                    &mut self.gpu.ib,
                    ib_bytes,
                    D3D11_BIND_INDEX_BUFFER.0 as u32,
                )?;

                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&vb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).context("Map(vb)")?;
                std::ptr::copy_nonoverlapping(
                    self.scratch_verts.as_ptr() as *const u8,
                    mapped.pData as *mut u8,
                    vb_bytes,
                );
                ctx.Unmap(&vb, 0);

                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&ib, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).context("Map(ib)")?;
                std::ptr::copy_nonoverlapping(
                    self.scratch_indices.as_ptr() as *const u8,
                    mapped.pData as *mut u8,
                    ib_bytes,
                );
                ctx.Unmap(&ib, 0);

                let stride = size_of::<epaint::Vertex>() as u32;
                let offset = 0u32;
                ctx.IASetInputLayout(&self.gpu.input_layout);
                ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.IASetVertexBuffers(0, 1, Some(&Some(vb)), Some(&stride), Some(&offset));
                ctx.IASetIndexBuffer(&ib, DXGI_FORMAT_R32_UINT, 0);
                ctx.VSSetShader(&self.gpu.vs, None);
                ctx.VSSetConstantBuffers(0, Some(&[Some(self.gpu.cbuf.clone())]));
                ctx.PSSetShader(&self.gpu.ps, None);
                ctx.RSSetState(&self.gpu.rast);
                ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: w as f32,
                    Height: h as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
                ctx.OMSetBlendState(&self.gpu.blend, None, u32::MAX);
                ctx.OMSetDepthStencilState(None, 0);
                ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);

                let mut bound_tex: Option<TextureId> = None;
                for call in &draws {
                    let Some(tex) = self.textures.get(&call.tex) else {
                        continue;
                    };
                    if bound_tex != Some(call.tex) {
                        let smp = get_or_create_sampler(&self.gpu.device, &mut self.gpu.samplers, tex.options)?;
                        ctx.PSSetShaderResources(0, Some(&[Some(tex.srv.clone())]));
                        ctx.PSSetSamplers(0, Some(&[Some(smp.clone())]));
                        bound_tex = Some(call.tex);
                    }
                    ctx.RSSetScissorRects(Some(&[call.scissor]));
                    ctx.DrawIndexed(call.index_count, call.index_offset, call.vertex_offset);
                }
                ctx.PSSetShaderResources(0, Some(&[None]));
                ctx.OMSetRenderTargets(Some(&[None]), None);
            }

            let hr = self.gpu.swap.Present(sync_interval, DXGI_PRESENT(0));
            if hr.is_err() {
                if is_device_lost(hr) {
                    self.recreate_after_device_lost()?;
                } else {
                    hr.ok().context("Present")?;
                }
            }
        }

        for id in &textures_delta.free {
            self.textures.remove(id);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// winit + egui-winit runner
// ---------------------------------------------------------------------------

struct ActiveHost<A: LobbyApp> {
    window: Arc<Window>,
    hwnd: HWND,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    viewport_info: egui::ViewportInfo,
    renderer: Renderer,
    app: A,
    next_repaint: Option<Instant>,
    /// Set right after `WindowEvent::Resized` synchronously repaints the new
    /// size so an immediately following `WM_PAINT` does not paint a duplicate
    /// frame unless egui or input actually asked for another one.
    skip_next_redraw: bool,
}

impl<A: LobbyApp> ActiveHost<A> {
    fn schedule_repaint(&mut self, when: Instant) {
        self.skip_next_redraw = false;
        self.next_repaint = Some(match self.next_repaint {
            Some(prev) => prev.min(when),
            None => when,
        });
    }

    fn is_window_drawable(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() && !IsIconic(self.hwnd).as_bool() }
    }

    fn paint(&mut self, event_loop: &ActiveEventLoop, in_resize: bool) {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        if let Err(e) = self.renderer.resize(size.width, size.height) {
            crate::log!("lobby resize failed: {e:#}");
            return;
        }

        egui_winit::update_viewport_info(&mut self.viewport_info, &self.egui_ctx, &self.window, false);
        let mut raw_input = self.egui_state.take_egui_input(&self.window);
        raw_input.viewports.insert(ViewportId::ROOT, self.viewport_info.clone());

        let app = &mut self.app;
        let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            app.ui(ui);
        });

        self.egui_state
            .handle_platform_output_with_event_loop(&self.window, event_loop, full_output.platform_output);

        let mut actions = Vec::new();
        let mut repaint_delay = Duration::MAX;
        if let Some(root) = full_output.viewport_output.get(&ViewportId::ROOT) {
            repaint_delay = root.repaint_delay;
            egui_winit::process_viewport_commands(
                &self.egui_ctx,
                &mut self.viewport_info,
                root.commands.iter().cloned(),
                &self.window,
                &mut actions,
            );
        }
        for action in actions {
            match action {
                egui_winit::ActionRequested::Cut => {
                    self.egui_state.egui_input_mut().events.push(egui::Event::Cut);
                }
                egui_winit::ActionRequested::Copy => {
                    self.egui_state.egui_input_mut().events.push(egui::Event::Copy);
                }
                egui_winit::ActionRequested::Paste => {
                    if let Some(contents) = self.egui_state.clipboard_text() {
                        let contents = contents.replace("\r\n", "\n");
                        if !contents.is_empty() {
                            self.egui_state.egui_input_mut().events.push(egui::Event::Paste(contents));
                        }
                    }
                }
                egui_winit::ActionRequested::Screenshot(_) => {}
            }
        }

        let ppp = full_output.pixels_per_point;
        let primitives = self.egui_ctx.tessellate(full_output.shapes, ppp);
        let clear = self.app.clear_color();
        // During live resize, Present(0, 0) copies synchronously to the DWM
        // redirection surface without stalling the modal drag loop; normal
        // frames VSync with Present(1, 0).
        let sync_interval = if in_resize { 0 } else { 1 };
        if let Err(e) = self.renderer.paint(clear, ppp, &full_output.textures_delta, &primitives, sync_interval) {
            crate::log!("lobby paint failed: {e:#}");
        }

        if repaint_delay == Duration::ZERO {
            self.schedule_repaint(Instant::now());
        } else if repaint_delay < Duration::from_secs(3600) {
            self.schedule_repaint(Instant::now() + repaint_delay);
        }
    }
}

struct Runner<A: LobbyApp, F: FnOnce(&egui::Context, HWND) -> A> {
    cfg: Config,
    proxy: EventLoopProxy<UserEvent>,
    init: Option<F>,
    host: Option<ActiveHost<A>>,
    init_err: Option<anyhow::Error>,
}

impl<A: LobbyApp, F: FnOnce(&egui::Context, HWND) -> A> Runner<A, F> {
    fn create_host(&mut self, event_loop: &ActiveEventLoop) -> Result<ActiveHost<A>> {
        let attrs = Window::default_attributes()
            .with_title(self.cfg.title)
            .with_inner_size(LogicalSize::new(self.cfg.inner_size[0], self.cfg.inner_size[1]))
            .with_min_inner_size(LogicalSize::new(self.cfg.min_inner_size[0], self.cfg.min_inner_size[1]))
            .with_theme(Some(Theme::Dark))
            .with_visible(false);
        let window = Arc::new(event_loop.create_window(attrs).context("create lobby window")?);

        let hwnd = match window.window_handle().context("window_handle")?.as_raw() {
            RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
            _ => bail!("expected Win32 window handle"),
        };

        install_no_erase(hwnd);
        theme::frame(hwnd, theme::CHROME);
        theme::set_icon(hwnd);
        super::tray::set_window(hwnd);

        let phys = window.inner_size();
        let renderer = Renderer::new(hwnd, (phys.width.max(1), phys.height.max(1)))?;

        let egui_ctx = egui::Context::default();
        let proxy = self.proxy.clone();
        egui_ctx.set_request_repaint_callback(move |info| {
            let when = Instant::now().checked_add(info.delay).unwrap_or_else(Instant::now);
            let _ = proxy.send_event(UserEvent::RequestRepaint { when });
        });

        let mut egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            Some(Theme::Dark),
            Some(renderer.max_texture_side()),
        );
        egui_state.init_accesskit(event_loop, &window, self.proxy.clone());

        let mut viewport_info = egui::ViewportInfo::default();
        egui_winit::update_viewport_info(&mut viewport_info, &egui_ctx, &window, true);

        let init = self.init.take().expect("init called once");
        let app = init(&egui_ctx, hwnd);

        let mut host = ActiveHost {
            window,
            hwnd,
            egui_ctx,
            egui_state,
            viewport_info,
            renderer,
            app,
            next_repaint: None,
            skip_next_redraw: false,
        };

        // Paint the first frame while hidden so the window appears already
        // coloured with zero white flash.
        host.paint(event_loop, false);
        if !self.cfg.hidden || super::tray::want_visible() {
            host.window.set_visible(true);
            host.window.request_redraw();
        }
        Ok(host)
    }
}

impl<A: LobbyApp, F: FnOnce(&egui::Context, HWND) -> A> ApplicationHandler<UserEvent> for Runner<A, F> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.host.is_some() {
            return;
        }
        match self.create_host(event_loop) {
            Ok(host) => self.host = Some(host),
            Err(e) => {
                self.init_err = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        match event {
            UserEvent::RequestRepaint { when } => host.schedule_repaint(when),
            UserEvent::AccessKit(ev) => match ev.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => {
                    host.egui_ctx.enable_accesskit();
                    host.schedule_repaint(Instant::now());
                }
                accesskit_winit::WindowEvent::ActionRequested(req) => {
                    host.egui_state.on_accesskit_action_request(req);
                    host.schedule_repaint(Instant::now());
                }
                accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                    host.egui_ctx.disable_accesskit();
                }
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                host.app.on_close(&host.egui_ctx);
            }
            WindowEvent::Resized(size) => {
                let _ = host.egui_state.on_window_event(&host.window, &event);
                if size.width > 0 && size.height > 0 {
                    host.paint(event_loop, true);
                    // Avoid an immediately redundant WM_PAINT for the same
                    // frame unless egui already scheduled an immediate repaint.
                    host.skip_next_redraw = host.next_repaint.is_none_or(|t| t > Instant::now());
                }
            }
            WindowEvent::RedrawRequested => {
                if host.skip_next_redraw {
                    host.skip_next_redraw = false;
                    return;
                }
                host.next_repaint = None;
                host.paint(event_loop, false);
            }
            other => {
                let resp = host.egui_state.on_window_event(&host.window, &other);
                if resp.repaint {
                    host.schedule_repaint(Instant::now());
                }
            }
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        if let DeviceEvent::MouseMotion { delta } = event
            && host.egui_state.on_mouse_motion(delta)
        {
            host.schedule_repaint(Instant::now());
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        host.skip_next_redraw = false;
        if let Some(when) = host.next_repaint {
            let now = Instant::now();
            if when <= now {
                host.next_repaint = None;
                if host.is_window_drawable() {
                    host.window.request_redraw();
                } else {
                    // Window is hidden in the tray or minimized: keep periodic
                    // state checks (e.g. self-update ready) without spinning.
                    host.next_repaint = Some(now + Duration::from_secs(1));
                }
            }
        }
        event_loop.set_control_flow(match host.next_repaint {
            Some(when) => ControlFlow::WaitUntil(when),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(host) = self.host.as_mut() {
            host.app.on_exit();
        }
    }
}

pub fn run<A, F>(cfg: Config, init: F) -> Result<()>
where
    A: LobbyApp,
    F: FnOnce(&egui::Context, HWND) -> A,
{
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().context("EventLoop::build")?;
    let proxy = event_loop.create_proxy();
    let mut runner = Runner { cfg, proxy, init: Some(init), host: None, init_err: None };
    event_loop.run_app(&mut runner).context("EventLoop::run_app")?;
    if let Some(err) = runner.init_err {
        return Err(err);
    }
    Ok(())
}
