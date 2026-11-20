//! Standalone, zero-overhead VSync-locked D3D11 visual benchmark pattern.
//!
//! Opens a borderless fullscreen (or centered 1280x720 windowed) D3D11 window
//! and renders a deterministic fullscreen-triangle pixel shader locked to the
//! monitor's native refresh rate via `Present(1, 0)`.

use std::{
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use windows::{
    Win32::{
        Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
                D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                Fxc::D3DCompile,
                ID3DBlob,
            },
            Direct3D11::{
                D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_MAP_WRITE_DISCARD,
                D3D11_MAPPED_SUBRESOURCE, D3D11_RASTERIZER_DESC, D3D11_SDK_VERSION, D3D11_USAGE_DYNAMIC,
                D3D11_VIEWPORT, D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
                ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11Texture2D,
                ID3D11VertexShader,
            },
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
                },
                CreateDXGIFactory1, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
                DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
                DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
                IDXGIAdapter1, IDXGIDevice1, IDXGIFactory1, IDXGIFactory2, IDXGISwapChain1,
            },
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Input::KeyboardAndMouse::VK_ESCAPE,
            WindowsAndMessaging::{
                AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                GetClientRect, GetSystemMetrics, IDC_ARROW, LoadCursorW, MSG, PM_REMOVE, PeekMessageW,
                PostQuitMessage, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SetForegroundWindow,
                ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
                WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WNDCLASSW, WS_CHILD, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
                WS_POPUP, WS_VISIBLE,
            },
        },
    },
    core::{Interface, PCSTR, s, w},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Light,
    Medium,
    Heavy,
}

impl Mode {
    fn as_u32(self) -> u32 {
        match self {
            Mode::Light => 0,
            Mode::Medium => 1,
            Mode::Heavy => 2,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Medium => "medium",
            Mode::Heavy => "heavy",
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "pattern", about = "120 Hz VSync-locked D3D11 visual benchmark pattern")]
struct Args {
    /// Visual workload complexity (light | medium | heavy)
    #[arg(long, value_enum, default_value_t = Mode::Light)]
    mode: Mode,

    /// Duration in seconds (0 = run until Escape or window closed)
    #[arg(long, default_value_t = 20)]
    seconds: u64,

    /// Run in a centered 1280x720 window instead of borderless fullscreen
    #[arg(long, default_value_t = false)]
    windowed: bool,

    /// Draw into a child window filling the top-level one (as the viewer does)
    #[arg(long, default_value_t = false)]
    child: bool,

    /// Present(0, ALLOW_TEARING), paced by a timer to 120 fps (as the viewer does)
    #[arg(long, default_value_t = false)]
    tearing: bool,

    /// Do not make the window topmost
    #[arg(long, default_value_t = false)]
    no_topmost: bool,

    /// Colour the frame with DwmSetWindowAttribute as the viewer does: all,
    /// or only one of dark, caption, border, corner
    #[arg(long)]
    frame: Option<String>,

    /// Windowed client size, e.g. 2472x1381 (default 1280x720)
    #[arg(long)]
    client: Option<String>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    frame: u32,
    mode: u32,
    width: f32,
    height: f32,
}

static SHOULD_QUIT: AtomicBool = AtomicBool::new(false);

const HLSL: &str = r#"
cbuffer Params : register(b0) {
    uint frame;
    uint mode;
    float width;
    float height;
};

struct VsOut {
    float4 pos : SV_Position;
};

VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

uint pcg_hash(uint v) {
    uint state = v * 747796405u + 2891336453u;
    uint word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// 3x5 bitmap font packed into 15 bits (row-major, bit 14 = top-left, bit 0 = bottom-right).
uint digit_glyph(uint d) {
    if (d == 0u) return 31599u; // 111 101 101 101 111
    if (d == 1u) return 11415u; // 010 110 010 010 111
    if (d == 2u) return 29671u; // 111 001 111 100 111
    if (d == 3u) return 29647u; // 111 001 111 001 111
    if (d == 4u) return 23497u; // 101 101 111 001 001
    if (d == 5u) return 31183u; // 111 100 111 001 111
    if (d == 6u) return 31215u; // 111 100 111 101 111
    if (d == 7u) return 29257u; // 111 001 001 001 001
    if (d == 8u) return 31727u; // 111 101 111 101 111
    return 31695u;              // 111 101 111 001 111 (9)
}

float3 hsv_to_rgb(float h, float s, float v) {
    float3 p = abs(frac(h + float3(0.0, 2.0 / 3.0, 1.0 / 3.0)) * 6.0 - 3.0);
    return v * lerp(float3(1.0, 1.0, 1.0), saturate(p - 1.0), s);
}

float3 draw_light(float2 p, uint2 px) {
    // Clean dark background + faint static 64px reference grid.
    float3 col = float3(0.055, 0.067, 0.090);
    if ((px.x & 63u) == 0u || (px.y & 63u) == 0u) {
        col += float3(0.025, 0.030, 0.040);
    }

    float f = (float)frame;

    // High-speed smooth moving vertical bars (makes any dropped frame obvious).
    float x1 = fmod(f * 9.0, width);
    float d1 = abs(p.x - x1);
    d1 = min(d1, width - d1);
    col = lerp(col, float3(0.13, 0.83, 0.93), smoothstep(18.0, 16.5, d1));

    float x2 = width - fmod(f * 13.5, width);
    float d2 = abs(p.x - x2);
    d2 = min(d2, width - d2);
    col = lerp(col, float3(0.96, 0.62, 0.04), smoothstep(11.0, 9.5, d2));

    float x3 = fmod(f * 21.0 + 300.0, width);
    float d3 = abs(p.x - x3);
    d3 = min(d3, width - d3);
    col = lerp(col, float3(0.93, 0.28, 0.60), smoothstep(6.0, 4.5, d3));

    // Moving window-like rectangle (simulates dragging a folder/window).
    float win_w = min(480.0, width * 0.40);
    float win_h = min(320.0, height * 0.42);
    float margin_x = max(16.0, (width - win_w) * 0.44);
    float margin_y = max(16.0, (height - win_h) * 0.38);
    float cx = width * 0.5 + sin(f * 0.037) * margin_x;
    float cy = height * 0.55 + sin(f * 0.053 + 0.8) * margin_y;
    float2 tl = float2(cx - win_w * 0.5, cy - win_h * 0.5);
    float2 lp = p - tl;

    if (lp.x >= 0.0 && lp.x < win_w && lp.y >= 0.0 && lp.y < win_h) {
        bool border = (lp.x < 2.0 || lp.x >= win_w - 2.0 || lp.y < 2.0 || lp.y >= win_h - 2.0);
        if (border) {
            col = float3(0.38, 0.65, 0.98);
        } else if (lp.y < 36.0) {
            // Window title bar.
            col = float3(0.12, 0.16, 0.24);
            // Traffic-light dots.
            if (length(lp - float2(20.0, 18.0)) < 6.0) col = float3(0.94, 0.27, 0.27);
            if (length(lp - float2(40.0, 18.0)) < 6.0) col = float3(0.96, 0.75, 0.14);
            if (length(lp - float2(60.0, 18.0)) < 6.0) col = float3(0.20, 0.83, 0.45);
            // Moving progress pill inside title bar.
            float pill_x = 90.0 + fmod(f * 6.0, max(1.0, win_w - 150.0));
            if (lp.y > 14.0 && lp.y < 22.0 && lp.x > pill_x && lp.x < pill_x + 40.0) {
                col = float3(0.38, 0.65, 0.98);
            }
        } else {
            // Window client area with simulated list rows.
            col = float3(0.08, 0.10, 0.15);
            float ry = lp.y - 48.0;
            if (ry >= 0.0 && lp.x > 20.0 && lp.x < win_w - 20.0) {
                uint row = (uint)(ry / 28.0);
                float in_row = ry - (float)row * 28.0;
                uint active_row = (frame / 15u) % 8u;
                if (in_row < 18.0 && row < 8u) {
                    float bar_len = 120.0 + (float)((row * 73u + 41u) % 200u);
                    if (lp.x < 24.0 + bar_len) {
                        col = (row == active_row) ? float3(0.22, 0.52, 0.92) : float3(0.22, 0.27, 0.36);
                    }
                }
            }
        }
    }

    return col;
}

float3 draw_medium(float2 p, uint2 px) {
    // Full-screen scrolling grid of sharp high-contrast blocks, diagonal stripes & gradients.
    uint sx = px.x + frame * 7u;
    uint sy = px.y + frame * 11u;
    uint tx = sx >> 6u;
    uint ty = sy >> 6u;
    uint lx = sx & 63u;
    uint ly = sy & 63u;

    uint cell_hash = pcg_hash(tx ^ (ty * 65521u));
    float hue = frac((float)(cell_hash & 255u) / 255.0 + (float)frame * 0.005);
    float3 tile_col = hsv_to_rgb(hue, 0.65, ((tx ^ ty) & 1u) != 0u ? 0.85 : 0.22);

    // Crisp 2px tile border.
    if (lx < 2u || ly < 2u) {
        return float3(0.95, 0.95, 0.98);
    }

    // Diagonal sharp stripes in alternating tiles, smooth gradient in others.
    if (((tx + ty) & 1u) == 0u) {
        uint diag = (sx + sy + frame * 5u) & 15u;
        return (diag < 5u) ? float3(0.95, 0.96, 1.0) : tile_col;
    } else {
        float grad = (float)lx / 63.0;
        return lerp(tile_col, float3(0.08, 0.10, 0.14), grad * 0.65);
    }
}

float3 draw_heavy(float2 p, uint2 px) {
    // 100% pixel-changing high-frequency procedural hash noise + checker + fast color bands.
    uint idx = px.x + px.y * 4096u + frame * 16777619u;
    uint h = pcg_hash(idx ^ pcg_hash(px.y + frame * 2654435761u));
    float3 noise = float3(
        (float)(h & 255u),
        (float)((h >> 8u) & 255u),
        (float)((h >> 16u) & 255u)
    ) / 255.0;

    float band_hue = frac((p.x + p.y * 0.5 + (float)frame * 19.0) / 360.0);
    float3 band = hsv_to_rgb(band_hue, 0.95, 1.0);
    float checker = (((px.x ^ px.y ^ frame) & 1u) != 0u) ? 1.0 : 0.25;

    return saturate(lerp(noise, band * checker, 0.42));
}

float3 draw_hud(float3 base, uint2 px) {
    // Top-left crisp frame-counter & binary frame-id HUD: [16, 16] .. [432, 92]
    if (px.x < 16u || px.x >= 432u || px.y < 16u || px.y >= 92u) {
        return base;
    }

    // 1px bright border.
    if (px.x == 16u || px.x == 431u || px.y == 16u || px.y == 91u) {
        return float3(0.45, 0.55, 0.68);
    }

    float3 col = float3(0.04, 0.05, 0.08);
    uint hx = px.x - 24u;
    uint hy = px.y - 24u;

    // Row 1 (hy 0..14): 32-bit binary frame ID (MSB left, LSB right).
    if (px.x >= 24u && hx < 384u && hy < 14u) {
        uint bit_idx = 31u - (hx / 12u);
        uint in_bit = hx % 12u;
        if (in_bit < 10u) {
            bool on = ((frame >> bit_idx) & 1u) != 0u;
            col = on ? float3(0.22, 0.85, 0.98) : float3(0.14, 0.18, 0.25);
        }
        return col;
    }

    // Row 2 (hy 20..32): frame-parity indicator + moving vernier sweep bar.
    if (px.x >= 24u && hx < 384u && hy >= 20u && hy < 32u) {
        if (hx < 14u) {
            // Toggles yellow/magenta every single frame.
            return ((frame & 1u) != 0u) ? float3(0.98, 0.80, 0.10) : float3(0.95, 0.25, 0.45);
        }
        if (hx >= 20u) {
            uint track_x = hx - 20u;
            uint track_w = 364u;
            uint pos = (frame * 8u) % track_w;
            uint dist = (track_x + track_w - pos) % track_w;
            if (dist < 18u) {
                return float3(0.29, 0.92, 0.50);
            }
            if (hy >= 28u && track_x == (frame % track_w)) {
                return float3(1.0, 1.0, 1.0);
            }
            return float3(0.12, 0.15, 0.21);
        }
        return col;
    }

    // Row 3 (hy 38..58): 6-digit decimal frame counter (3x5 font scaled 4x -> 12x20 px, 4px gap).
    if (px.x >= 24u && hx < 96u && hy >= 38u && hy < 58u) {
        uint digit_slot = hx / 16u;
        uint dx = hx % 16u;
        uint dy = hy - 38u;
        if (digit_slot < 6u && dx < 12u) {
            uint div = 1u;
            if (digit_slot == 0u) div = 100000u;
            else if (digit_slot == 1u) div = 10000u;
            else if (digit_slot == 2u) div = 1000u;
            else if (digit_slot == 3u) div = 100u;
            else if (digit_slot == 4u) div = 10u;
            uint digit = (frame / div) % 10u;
            uint gx = dx / 4u;
            uint gy = dy / 4u;
            uint glyph = digit_glyph(digit);
            uint bit = (4u - gy) * 3u + (2u - gx);
            if (((glyph >> bit) & 1u) != 0u) {
                return float3(0.97, 0.98, 1.00);
            }
        }
        return col;
    }

    return col;
}

float4 ps_main(VsOut i) : SV_Target {
    uint2 px = uint2(i.pos.xy);
    float3 col;
    if (mode == 0u) {
        col = draw_light(i.pos.xy, px);
    } else if (mode == 1u) {
        col = draw_medium(i.pos.xy, px);
    } else {
        col = draw_heavy(i.pos.xy, px);
    }
    col = draw_hud(col, px);
    return float4(col, 1.0);
}
"#;

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wparam.0 == VK_ESCAPE.0 as usize {
                SHOULD_QUIT.store(true, Ordering::Relaxed);
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            SHOULD_QUIT.store(true, Ordering::Relaxed);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            SHOULD_QUIT.store(true, Ordering::Relaxed);
            unsafe {
                PostQuitMessage(0);
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => {
            return LRESULT(1);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn compile_shader(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
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
            bail!("D3DCompile failed: {e} {}", msg.unwrap_or_default());
        }
        code.context("D3DCompile returned no bytecode")
    }
}

fn pick_primary_adapter() -> Result<IDXGIAdapter1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                j += 1;
                let desc = output.GetDesc()?;
                let r = desc.DesktopCoordinates;
                if desc.AttachedToDesktop.as_bool() && r.left == 0 && r.top == 0 {
                    return Ok(adapter);
                }
            }
        }
        factory.EnumAdapters1(0).context("no DXGI graphics adapter found")
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
}

fn create_rtv(device: &ID3D11Device, swap: &IDXGISwapChain1) -> Result<ID3D11RenderTargetView> {
    unsafe {
        let back: ID3D11Texture2D = swap.GetBuffer(0).context("swap.GetBuffer(0)")?;
        let mut rtv = None;
        device
            .CreateRenderTargetView(&back, None, Some(&mut rtv))
            .context("CreateRenderTargetView")?;
        rtv.context("no RTV created")
    }
}

fn percentile_95(samples: &[f64]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let max = *sorted.last().unwrap();
    let idx = ((sorted.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    (sorted[idx], max)
}

fn main() -> Result<()> {
    let args = Args::parse();

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let (screen_w, screen_h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if screen_w <= 0 || screen_h <= 0 {
        bail!("invalid screen dimensions: {screen_w}x{screen_h}");
    }

    let hwnd = unsafe {
        let inst = GetModuleHandleW(None).context("GetModuleHandleW")?;
        let class_name = w!("rc_pattern_bench");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: inst.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let (style, x, y, win_w, win_h) = if args.windowed {
            let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
            let (cw, ch) = args
                .client
                .as_deref()
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?)))
                .unwrap_or((1280, 720));
            let client_w = cw.min(screen_w);
            let client_h = ch.min(screen_h);
            let mut r = RECT { left: 0, top: 0, right: client_w, bottom: client_h };
            AdjustWindowRectEx(&mut r, style, false, WINDOW_EX_STYLE::default())
                .context("AdjustWindowRectEx")?;
            let ww = r.right - r.left;
            let wh = r.bottom - r.top;
            let wx = ((screen_w - ww) / 2).max(0);
            let wy = ((screen_h - wh) / 2).max(0);
            (style, wx, wy, ww, wh)
        } else {
            (WS_POPUP | WS_VISIBLE, 0, 0, screen_w, screen_h)
        };

        let title = w!("remote-control pattern benchmark");
        // Topmost: started from a background process, the window would
        // otherwise open behind whatever has the focus and never be composed.
        let hwnd = CreateWindowExW(
            if args.no_topmost { WINDOW_EX_STYLE::default() } else { WS_EX_TOPMOST },
            class_name,
            title,
            style,
            x,
            y,
            win_w,
            win_h,
            None,
            None,
            Some(inst.into()),
            None,
        )
        .context("CreateWindowExW")?;

        if let Some(which) = args.frame.as_deref() {
            use windows::Win32::Graphics::Dwm::{
                DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
            };
            let set = |name: &str, attr, v: u32| {
                if which == "all" || which == name {
                    let _ = DwmSetWindowAttribute(hwnd, attr, &v as *const u32 as *const _, 4);
                }
            };
            set("dark", DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
            set("caption", DWMWA_CAPTION_COLOR, 0x0020_1c1b);
            set("border", DWMWA_BORDER_COLOR, 0x0038_3331);
            set("corner", DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0 as u32);
            if which == "square" {
                let v = windows::Win32::Graphics::Dwm::DWMWCP_DONOTROUND.0 as u32;
                let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &v as *const u32 as *const _, 4);
            }
        }

        // The first ShowWindow takes the show state from STARTUPINFO, which is
        // SW_HIDE when started from a hidden console (remote `exec`): show twice.
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        if args.child {
            let (cw, ch) = client_size(hwnd);
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("pattern child"),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                cw as i32,
                ch as i32,
                Some(hwnd),
                None,
                Some(inst.into()),
                None,
            )
            .context("CreateWindowExW(child)")?
        } else {
            hwnd
        }
    };

    let mut size = client_size(hwnd);

    // D3D11 device + flip-model swap chain setup.
    let (device, ctx, swap, mut rtv, vs, ps, rast, cb) = unsafe {
        let adapter = pick_primary_adapter()?;
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .context("D3D11CreateDevice")?;
        let device = device.context("no ID3D11Device")?;
        let ctx = ctx.context("no ID3D11DeviceContext")?;

        let factory: IDXGIFactory2 = adapter.GetParent().context("adapter.GetParent(IDXGIFactory2)")?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0,
            Height: size.1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: if args.tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 },
            ..Default::default()
        };
        let swap = factory
            .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
            .context("CreateSwapChainForHwnd")?;
        let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
        if let Ok(dxgi1) = device.cast::<IDXGIDevice1>() {
            let _ = dxgi1.SetMaximumFrameLatency(1);
        }

        let rtv = create_rtv(&device, &swap)?;

        let vs_blob = compile_shader(s!("vs_main"), s!("vs_5_0"))?;
        let ps_blob = compile_shader(s!("ps_main"), s!("ps_5_0"))?;
        let vs_bytes = std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
        let ps_bytes = std::slice::from_raw_parts(ps_blob.GetBufferPointer() as *const u8, ps_blob.GetBufferSize());

        let mut vs: Option<ID3D11VertexShader> = None;
        device.CreateVertexShader(vs_bytes, None, Some(&mut vs)).context("CreateVertexShader")?;
        let mut ps: Option<ID3D11PixelShader> = None;
        device.CreatePixelShader(ps_bytes, None, Some(&mut ps)).context("CreatePixelShader")?;

        let rd = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: false.into(),
            ..Default::default()
        };
        let mut rast: Option<ID3D11RasterizerState> = None;
        device.CreateRasterizerState(&rd, Some(&mut rast)).context("CreateRasterizerState")?;

        let cbd = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<Params>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cb: Option<ID3D11Buffer> = None;
        device.CreateBuffer(&cbd, None, Some(&mut cb)).context("CreateBuffer(cb)")?;

        (
            device,
            ctx,
            swap,
            rtv,
            vs.context("no vs")?,
            ps.context("no ps")?,
            rast.context("no rast")?,
            cb.context("no cb")?,
        )
    };

    println!(
        "pattern: mode={} {}x{} ({}) seconds={}",
        args.mode.as_str(),
        size.0,
        size.1,
        if args.windowed { "windowed" } else { "borderless-fullscreen" },
        if args.seconds == 0 { "unlimited".to_string() } else { args.seconds.to_string() },
    );
    let _ = std::io::stdout().flush();

    let limit = (args.seconds > 0).then(|| Duration::from_secs(args.seconds));
    let swap_flags =
        if args.tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
    let start = Instant::now();
    let mut tick = start;
    let mut last_report = start;
    let mut prev_present: Option<Instant> = None;
    let mut frame: u32 = 0;
    let mut sec_frames: u64 = 0;
    let mut sec_deltas: Vec<f64> = Vec::with_capacity(256);
    let mut all_deltas: Vec<f64> = Vec::with_capacity(16384);

    let mut msg = MSG::default();
    loop {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    SHOULD_QUIT.store(true, Ordering::Relaxed);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if SHOULD_QUIT.load(Ordering::Relaxed) {
            break;
        }
        if let Some(dur) = limit
            && start.elapsed() >= dur
        {
            break;
        }

        // Handle resize cleanly in windowed mode.
        let now_size = client_size(hwnd);
        if now_size != size && now_size.0 > 0 && now_size.1 > 0 {
            drop(rtv);
            unsafe {
                swap.ResizeBuffers(0, now_size.0, now_size.1, DXGI_FORMAT_UNKNOWN, swap_flags)
                    .context("ResizeBuffers")?;
            }
            rtv = create_rtv(&device, &swap)?;
            size = now_size;
        }

        // Update 16-byte constant buffer.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .context("Map(cb)")?;
            let dst = mapped.pData as *mut Params;
            *dst = Params {
                frame,
                mode: args.mode.as_u32(),
                width: size.0 as f32,
                height: size.1 as f32,
            };
            ctx.Unmap(&cb, 0);

            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: size.0 as f32,
                Height: size.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&vs, None);
            ctx.PSSetShader(&ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            ctx.RSSetState(&rast);
            ctx.RSSetViewports(Some(&[vp]));
            ctx.OMSetBlendState(None, None, u32::MAX);
            ctx.OMSetDepthStencilState(None, 0);
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.Draw(3, 0);

            if args.tearing {
                // As the viewer: present at once, at the stream's rate (120 fps).
                tick += Duration::from_micros(8333);
                let now = Instant::now();
                if tick > now {
                    std::thread::sleep(tick - now);
                } else {
                    tick = now;
                }
                swap.Present(0, DXGI_PRESENT_ALLOW_TEARING).ok().context("Present(0, tearing)")?;
            } else {
                swap.Present(1, DXGI_PRESENT(0)).ok().context("Present(1, 0)")?;
            }
        }

        let now = Instant::now();
        if let Some(prev) = prev_present {
            let dt_ms = (now - prev).as_secs_f64() * 1000.0;
            sec_deltas.push(dt_ms);
            all_deltas.push(dt_ms);
        }
        prev_present = Some(now);
        frame = frame.wrapping_add(1);
        sec_frames += 1;

        let report_dt = now - last_report;
        if report_dt >= Duration::from_secs(1) {
            let total_s = (now - start).as_secs_f64();
            let interval_fps = sec_frames as f64 / report_dt.as_secs_f64();
            let mean_fps = frame as f64 / total_s;
            let (p95_ms, max_ms) = percentile_95(&sec_deltas);
            println!(
                "[{total_s:5.2}s] frames={frame:<6} interval_fps={interval_fps:6.2} mean_fps={mean_fps:6.2} p95_ms={p95_ms:5.2} max_ms={max_ms:5.2}"
            );
            let _ = std::io::stdout().flush();
            sec_frames = 0;
            sec_deltas.clear();
            last_report = now;
        }
    }

    let elapsed_s = start.elapsed().as_secs_f64();
    let mean_fps = if elapsed_s > 0.0 { frame as f64 / elapsed_s } else { 0.0 };
    let (p95_ms, max_ms) = percentile_95(&all_deltas);
    println!(
        "DONE mode={} frames={} elapsed={:.3}s mean_fps={:.2} p95_ms={:.2} max_ms={:.2}",
        args.mode.as_str(),
        frame,
        elapsed_s,
        mean_fps,
        p95_ms,
        max_ms
    );
    let _ = std::io::stdout().flush();

    unsafe {
        let _ = DestroyWindow(hwnd);
    }

    Ok(())
}
