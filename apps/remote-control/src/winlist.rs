//! Enumerates real visible top-level user windows on the Windows host desktop,
//! extracts high-DPI process/window icons and live window thumbnails as PNG bytes,
//! and focuses/adapts/restores a chosen window for exclusive mobile single-window streaming.

use std::path::Path;

use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, RECT, SIZE, WPARAM},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            Gdi::{
                BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap,
                CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
                GetDC, GetDIBits, GetObjectW, HALFTONE, HBITMAP, HGDIOBJ, ReleaseDC, SRCCOPY,
                SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt,
            },
        },
        Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Threading::{
                AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
        UI::{
            Shell::{
                IShellItemImageFactory, SHCreateItemFromParsingName, SHFILEINFOW, SHGFI_ICON,
                SHGFI_LARGEICON, SHGetFileInfoW, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
            },
            WindowsAndMessaging::{
                BringWindowToTop, DI_NORMAL, DestroyIcon, DrawIconEx, EnumChildWindows,
                EnumWindows, GCLP_HICON, GCLP_HICONSM, GWL_EXSTYLE, GW_OWNER, GetClassLongPtrW,
                GetClassNameW, GetForegroundWindow, GetIconInfo, GetWindow, GetWindowLongPtrW,
                GetWindowPlacement, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
                GetWindowThreadProcessId, HICON, ICONINFO, ICON_BIG, ICON_SMALL2, IsIconic,
                IsWindow, IsWindowVisible, IsZoomed, PrivateExtractIconsW, SMTO_ABORTIFHUNG,
                SW_MAXIMIZE, SW_RESTORE, SW_SHOW, SWP_NOZORDER, SendMessageTimeoutW,
                SetForegroundWindow, SetWindowPlacement, SetWindowPos, ShowWindow, WINDOWPLACEMENT,
                WM_GETICON, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
            },
        },
    },
    core::{BOOL, HSTRING, PWSTR},
};

use crate::proto::{MonitorDesc, WindowDesc};

const MAX_WINDOWS: usize = 48;
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
const ICON_FETCH_SIZE: i32 = 256;
const ICON_OUT_SIZE: u32 = 128;
const THUMB_MAX_W: i32 = 360;
const THUMB_MAX_H: i32 = 225;

#[derive(Clone, Copy)]
pub struct SavedWindow {
    pub hwnd: u64,
    placement: WINDOWPLACEMENT,
}

// `WINDOWPLACEMENT` is plain Win32 POD structs/ints.
unsafe impl Send for SavedWindow {}
unsafe impl Sync for SavedWindow {}

/// Enumerates visible top-level application windows in Z-order (most recent first).
pub fn enumerate(monitors: &[MonitorDesc]) -> Vec<WindowDesc> {
    let _ = crate::desktop::follow();
    struct Ctx<'a> {
        monitors: &'a [MonitorDesc],
        out: Vec<WindowDesc>,
    }
    let mut ctx = Ctx { monitors, out: Vec::with_capacity(24) };

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx<'_>) };
        if ctx.out.len() >= MAX_WINDOWS {
            return BOOL(0);
        }
        if let Some(desc) = inspect_window(hwnd, ctx.monitors) {
            ctx.out.push(desc);
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut Ctx<'_> as isize));
    }
    ctx.out
}

/// Returns true if `hwnd_raw` is currently a valid, visible, non-minimized window
/// large enough to capture with WGC.
pub fn is_window_capturable(hwnd_raw: u64) -> bool {
    if hwnd_raw == 0 {
        return false;
    }
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
    }
    let Some(r) = window_capture_rect(hwnd_raw) else { return false };
    (r.right - r.left) >= 64 && (r.bottom - r.top) >= 64
}

/// The visible desktop rectangle of `hwnd_raw` (using `DWMWA_EXTENDED_FRAME_BOUNDS`
/// to exclude invisible Win10/11 drop-shadow borders so pointer mapping aligns 1:1
/// with WGC's captured window image).
pub fn window_capture_rect(hwnd_raw: u64) -> Option<RECT> {
    if hwnd_raw == 0 {
        return None;
    }
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_ok()
            && r.right > r.left
            && r.bottom > r.top
        {
            return Some(r);
        }
        if GetWindowRect(hwnd, &mut r).is_ok() && r.right > r.left && r.bottom > r.top {
            return Some(r);
        }
    }
    None
}

/// Picks the window rectangle (`DWMWA_EXTENDED_FRAME_BOUNDS` vs `GetWindowRect`)
/// whose dimensions match the actual WGC capture size `(cap_w, cap_h)` so
/// pointer injection aligns 1:1 with the streamed window pixels.
pub fn window_rect_matching_size(hwnd_raw: u64, cap_w: u32, cap_h: u32) -> Option<RECT> {
    if hwnd_raw == 0 {
        return None;
    }
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        let mut ext = RECT::default();
        let has_ext = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut ext as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_ok()
            && ext.right > ext.left
            && ext.bottom > ext.top;

        let mut win = RECT::default();
        let has_win = GetWindowRect(hwnd, &mut win).is_ok()
            && win.right > win.left
            && win.bottom > win.top;

        if has_ext && has_win && cap_w > 0 && cap_h > 0 {
            let ext_err = (ext.right - ext.left).abs_diff(cap_w as i32)
                + (ext.bottom - ext.top).abs_diff(cap_h as i32);
            let win_err = (win.right - win.left).abs_diff(cap_w as i32)
                + (win.bottom - win.top).abs_diff(cap_h as i32);
            return Some(if win_err + 1 < ext_err { win } else { ext });
        }
        if has_ext {
            return Some(ext);
        }
        if has_win {
            return Some(win);
        }
    }
    None
}

/// Captures the original placement of `hwnd_raw` before single-window resizing.
#[allow(dead_code)]
pub fn capture_placement(hwnd_raw: u64) -> Option<SavedWindow> {
    if hwnd_raw == 0 {
        return None;
    }
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        let mut wp = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut wp).is_ok() {
            Some(SavedWindow { hwnd: hwnd_raw, placement: wp })
        } else {
            None
        }
    }
}

/// Restores a window back to its pre-teleport desktop position/maximized state.
pub fn restore_placement(saved: SavedWindow) {
    let _ = crate::desktop::follow();
    let hwnd = HWND(saved.hwnd as usize as *mut _);
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            if IsZoomed(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetWindowPlacement(hwnd, &saved.placement);
        }
    }
}

/// Brings `hwnd_raw` to foreground without resizing or moving it.
pub fn bring_to_front(hwnd_raw: u64) -> bool {
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }

        let fg = GetForegroundWindow();
        let cur_tid = GetCurrentThreadId();
        let fg_tid = if !fg.is_invalid() {
            GetWindowThreadProcessId(fg, None)
        } else {
            0
        };
        let target_tid = GetWindowThreadProcessId(hwnd, None);

        let attached_fg = fg_tid != 0 && fg_tid != cur_tid
            && AttachThreadInput(cur_tid, fg_tid, true).as_bool();
        let attached_target = target_tid != 0 && target_tid != cur_tid && target_tid != fg_tid
            && AttachThreadInput(cur_tid, target_tid, true).as_bool();

        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);

        if attached_target {
            let _ = AttachThreadInput(cur_tid, target_tid, false);
        }
        if attached_fg {
            let _ = AttachThreadInput(cur_tid, fg_tid, false);
        }
        true
    }
}

/// Focuses `hwnd_raw` on its current physical monitor without modifying its size/position.
/// Returns the monitor index containing it.
pub fn focus_window_only(hwnd_raw: u64, monitors: &[MonitorDesc]) -> Option<u8> {
    if !bring_to_front(hwnd_raw) {
        return None;
    }
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    window_capture_rect(hwnd_raw)
        .or_else(|| window_rect(hwnd, false))
        .map(|r| monitor_for_rect(&r, monitors))
}

/// Teleports `hwnd_raw` onto `target_rect` (typically the per-session phone virtual
/// display) and maximizes it so the entire virtual monitor cleanly houses the window
/// plus all its child menus/dialogs/tooltips. Returns the window's resulting visible
/// `(width, height)` so callers can detect `WM_GETMINMAXINFO` overflow.
pub fn teleport_window_to_monitor(hwnd_raw: u64, target_rect: RECT) -> Option<(u32, u32)> {
    if !bring_to_front(hwnd_raw) {
        return None;
    }
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    let mon_w = (target_rect.right - target_rect.left).max(320);
    let mon_h = (target_rect.bottom - target_rect.top).max(320);
    unsafe {
        if IsZoomed(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        // Move into target monitor first so WM_DPICHANGED fires and Windows associates
        // the window with the virtual display before maximizing.
        let _ = SetWindowPos(
            hwnd,
            None,
            target_rect.left,
            target_rect.top,
            mon_w,
            mon_h,
            SWP_NOZORDER,
        );
        std::thread::sleep(std::time::Duration::from_millis(45));
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        std::thread::sleep(std::time::Duration::from_millis(45));
    }
    window_capture_rect(hwnd_raw)
        .or_else(|| window_rect(hwnd, false))
        .map(|r| ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32))
}

/// Extracts a sharp high-DPI Windows icon for `hwnd_raw` (preferring 256×256
/// Shell/PE resources downsampled smoothly to 128×128 RGBA PNG with trimmed padding).
pub fn extract_app_icon_png(hwnd_raw: u64) -> Option<Vec<u8>> {
    if hwnd_raw == 0 {
        return None;
    }
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }

        // 1. Prefer high-res Shell/PE icon from the real executable path.
        if let Some(exe) = real_window_exe_path(hwnd) {
            if let Some((rgba, w, h)) = extract_shell_bitmap_rgba(&exe, ICON_FETCH_SIZE) {
                if let Some(png) = finalize_icon_png(&rgba, w, h, ICON_OUT_SIZE) {
                    return Some(png);
                }
            }
            if let Some((rgba, w, h)) = extract_pe_hicon_rgba(&exe, ICON_FETCH_SIZE) {
                if let Some(png) = finalize_icon_png(&rgba, w, h, ICON_OUT_SIZE) {
                    return Some(png);
                }
            }
        }

        // 2. Fallback to window/class HICON or SHGetFileInfoW.
        let mut owned = false;
        let mut hicon = query_window_hicon(hwnd);
        if hicon.is_invalid() {
            if let Some(exe) = real_window_exe_path(hwnd) {
                if let Some(sh_icon) = exe_hicon(&exe) {
                    hicon = sh_icon;
                    owned = true;
                }
            }
        }
        if hicon.is_invalid() {
            return None;
        }
        let rgba_opt = render_hicon_to_rgba(hicon, ICON_OUT_SIZE as i32);
        if owned {
            let _ = DestroyIcon(hicon);
        }
        rgba_opt.and_then(|rgba| finalize_icon_png(&rgba, ICON_OUT_SIZE, ICON_OUT_SIZE, ICON_OUT_SIZE))
    }
}

unsafe fn extract_shell_bitmap_rgba(exe_path: &str, size: i32) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let hr_com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let com_ok = hr_com.is_ok();
        let wide = HSTRING::from(exe_path);
        let factory: Result<IShellItemImageFactory, _> = SHCreateItemFromParsingName(&wide, None);
        let res = match factory {
            Ok(f) => f
                .GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT)
                .ok()
                .and_then(|hbmp| {
                    let out = read_hbitmap_rgba(hbmp);
                    let _ = DeleteObject(HGDIOBJ(hbmp.0));
                    out
                }),
            Err(_) => None,
        };
        if com_ok {
            CoUninitialize();
        }
        res
    }
}

unsafe fn extract_pe_hicon_rgba(exe_path: &str, size: i32) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let mut path_buf = [0u16; 260];
        let utf16: Vec<u16> = exe_path.encode_utf16().collect();
        if utf16.is_empty() || utf16.len() >= path_buf.len() {
            return None;
        }
        path_buf[..utf16.len()].copy_from_slice(&utf16);
        let mut icons = [HICON::default(); 1];
        let mut id = 0u32;
        let count = PrivateExtractIconsW(
            &path_buf,
            0,
            size,
            size,
            Some(&mut icons),
            Some(&mut id),
            0,
        );
        if count == 0 || icons[0].is_invalid() {
            return None;
        }
        let hicon = icons[0];
        let rgba = render_hicon_to_rgba(hicon, size);
        let _ = DestroyIcon(hicon);
        rgba.map(|buf| (buf, size as u32, size as u32))
    }
}

unsafe fn read_hbitmap_rgba(hbmp: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(hbmp.0),
            size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        ) <= 0
            || bm.bmWidth <= 0
            || bm.bmHeight <= 0
        {
            return None;
        }
        let w = bm.bmWidth;
        let h = bm.bmHeight;
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return None;
        }
        let dc = CreateCompatibleDC(Some(screen_dc));
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut raw = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            dc,
            hbmp,
            0,
            h as u32,
            Some(raw.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        let _ = ReleaseDC(None, screen_dc);
        if lines == 0 {
            return None;
        }
        let mut rgba = vec![0u8; raw.len()];
        let mut any_alpha = false;
        let mut any_color = false;
        for (src, dst) in raw.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            // Un-premultiply if alpha > 0? Note IShellItemImageFactory uses premultiplied or straight alpha;
            // keep straight or cleanly un-premultiplied RGB to avoid dark fringes on translucent edges.
            let a = src[3];
            if a != 0 {
                any_alpha = true;
                if a < 255 {
                    let af = a as f32 / 255.0;
                    dst[0] = ((src[2] as f32 / af).min(255.0)) as u8;
                    dst[1] = ((src[1] as f32 / af).min(255.0)) as u8;
                    dst[2] = ((src[0] as f32 / af).min(255.0)) as u8;
                } else {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                }
                dst[3] = a;
            }
            if src[0] != 0 || src[1] != 0 || src[2] != 0 {
                any_color = true;
            }
        }
        if !any_alpha && !any_color {
            return None;
        }
        if !any_alpha {
            for (src, dst) in raw.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = if src[0] != 0 || src[1] != 0 || src[2] != 0 { 255 } else { 0 };
            }
        }
        Some((rgba, w as u32, h as u32))
    }
}

/// Crops excess transparent borders from `rgba` (`src_w x src_h`) and smoothly
/// bilinear-resamples into a centered `out_size x out_size` PNG with 4% padding.
fn finalize_icon_png(rgba: &[u8], src_w: u32, src_h: u32, out_size: u32) -> Option<Vec<u8>> {
    if src_w == 0 || src_h == 0 || rgba.len() != (src_w * src_h * 4) as usize {
        return None;
    }
    let (mut min_x, mut min_y) = (src_w, src_h);
    let (mut max_x, mut max_y) = (0u32, 0u32);
    for y in 0..src_h {
        for x in 0..src_w {
            let a = rgba[((y * src_w + x) * 4 + 3) as usize];
            if a > 8 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if max_x < min_x || max_y < min_y {
        return None;
    }
    let cw = (max_x - min_x + 1) as f64;
    let ch = (max_y - min_y + 1) as f64;
    let content_max = cw.max(ch);
    // Keep 4% breathing room inside the target square.
    let target_span = out_size as f64 * 0.92;
    let scale = target_span / content_max;
    let dst_w = cw * scale;
    let dst_h = ch * scale;
    let off_x = (out_size as f64 - dst_w) * 0.5;
    let off_y = (out_size as f64 - dst_h) * 0.5;

    let mut out = vec![0u8; (out_size * out_size * 4) as usize];
    for dy in 0..out_size {
        for dx in 0..out_size {
            let sx = (dx as f64 + 0.5 - off_x) / scale + min_x as f64 - 0.5;
            let sy = (dy as f64 + 0.5 - off_y) / scale + min_y as f64 - 0.5;
            if sx < min_x as f64 - 0.5 || sx > max_x as f64 + 0.5 || sy < min_y as f64 - 0.5 || sy > max_y as f64 + 0.5 {
                continue;
            }
            let x0 = (sx.floor() as i32).clamp(0, src_w as i32 - 1) as u32;
            let y0 = (sy.floor() as i32).clamp(0, src_h as i32 - 1) as u32;
            let x1 = (x0 + 1).min(src_w - 1);
            let y1 = (y0 + 1).min(src_h - 1);
            let fx = (sx - sx.floor()).clamp(0.0, 1.0);
            let fy = (sy - sy.floor()).clamp(0.0, 1.0);

            let sample = |x: u32, y: u32| -> [f64; 4] {
                let idx = ((y * src_w + x) * 4) as usize;
                let a = rgba[idx + 3] as f64;
                // Premultiply during interpolation to prevent dark halo bleeding.
                [rgba[idx] as f64 * a, rgba[idx + 1] as f64 * a, rgba[idx + 2] as f64 * a, a]
            };
            let p00 = sample(x0, y0);
            let p10 = sample(x1, y0);
            let p01 = sample(x0, y1);
            let p11 = sample(x1, y1);
            let w00 = (1.0 - fx) * (1.0 - fy);
            let w10 = fx * (1.0 - fy);
            let w01 = (1.0 - fx) * fy;
            let w11 = fx * fy;

            let a = p00[3] * w00 + p10[3] * w10 + p01[3] * w01 + p11[3] * w11;
            let oidx = ((dy * out_size + dx) * 4) as usize;
            if a > 0.5 {
                out[oidx] = ((p00[0] * w00 + p10[0] * w10 + p01[0] * w01 + p11[0] * w11) / a).round().clamp(0.0, 255.0) as u8;
                out[oidx + 1] = ((p00[1] * w00 + p10[1] * w10 + p01[1] * w01 + p11[1] * w11) / a).round().clamp(0.0, 255.0) as u8;
                out[oidx + 2] = ((p00[2] * w00 + p10[2] * w10 + p01[2] * w01 + p11[2] * w11) / a).round().clamp(0.0, 255.0) as u8;
                out[oidx + 3] = a.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    encode_rgba_png(&out, out_size, out_size)
}

/// Captures a real live thumbnail of `hwnd_raw` via `PrintWindow(PW_RENDERFULLCONTENT)`
/// scaled down with `HALFTONE` filtering and encoded as a fast RGB PNG.
pub fn capture_window_thumb_png(hwnd_raw: u64) -> Option<Vec<u8>> {
    if hwnd_raw == 0 {
        return None;
    }
    let _ = crate::desktop::follow();
    let hwnd = HWND(hwnd_raw as usize as *mut _);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return None;
        }
        let sw = wr.right - wr.left;
        let sh = wr.bottom - wr.top;
        if sw < 64 || sh < 48 || sw > 16_000 || sh > 16_000 {
            return None;
        }

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return None;
        }
        let src_dc = CreateCompatibleDC(Some(screen_dc));
        let src_bmp = CreateCompatibleBitmap(screen_dc, sw, sh);
        if src_dc.is_invalid() || src_bmp.is_invalid() {
            if !src_bmp.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(src_bmp.0));
            }
            if !src_dc.is_invalid() {
                let _ = DeleteDC(src_dc);
            }
            let _ = ReleaseDC(None, screen_dc);
            return None;
        }
        let old_src = SelectObject(src_dc, HGDIOBJ(src_bmp.0));

        let mut ok = PrintWindow(hwnd, src_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
        if !ok {
            ok = PrintWindow(hwnd, src_dc, PRINT_WINDOW_FLAGS(0)).as_bool();
        }
        if !ok {
            SelectObject(src_dc, old_src);
            let _ = DeleteObject(HGDIOBJ(src_bmp.0));
            let _ = DeleteDC(src_dc);
            let _ = ReleaseDC(None, screen_dc);
            return None;
        }

        let (crop_x, crop_y, crop_w, crop_h) = match window_capture_rect(hwnd_raw) {
            Some(er) => {
                let cx = (er.left - wr.left).clamp(0, sw - 1);
                let cy = (er.top - wr.top).clamp(0, sh - 1);
                let cw = (er.right - er.left).clamp(1, sw - cx);
                let ch = (er.bottom - er.top).clamp(1, sh - cy);
                (cx, cy, cw, ch)
            }
            None => (0, 0, sw, sh),
        };

        let scale = (THUMB_MAX_W as f64 / crop_w as f64)
            .min(THUMB_MAX_H as f64 / crop_h as f64)
            .min(1.0);
        let tw = ((crop_w as f64 * scale).round() as i32).max(2) & !1;
        let th = ((crop_h as f64 * scale).round() as i32).max(2) & !1;

        let dst_dc = CreateCompatibleDC(Some(screen_dc));
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dst_bmp = CreateDIBSection(Some(dst_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let result = if let Ok(dst_bmp) = dst_bmp
            && !dst_dc.is_invalid()
            && !bits.is_null()
        {
            let old_dst = SelectObject(dst_dc, HGDIOBJ(dst_bmp.0));
            let _ = SetStretchBltMode(dst_dc, HALFTONE);
            let _ = SetBrushOrgEx(dst_dc, 0, 0, None);
            let blt_ok = StretchBlt(
                dst_dc, 0, 0, tw, th, Some(src_dc), crop_x, crop_y, crop_w, crop_h, SRCCOPY,
            )
            .as_bool();
            let png = if blt_ok {
                let raw = std::slice::from_raw_parts(bits as *const u8, (tw * th * 4) as usize);
                encode_bgra_as_rgb_png(raw, tw as u32, th as u32)
            } else {
                None
            };
            SelectObject(dst_dc, old_dst);
            let _ = DeleteObject(HGDIOBJ(dst_bmp.0));
            png
        } else {
            None
        };

        if !dst_dc.is_invalid() {
            let _ = DeleteDC(dst_dc);
        }
        SelectObject(src_dc, old_src);
        let _ = DeleteObject(HGDIOBJ(src_bmp.0));
        let _ = DeleteDC(src_dc);
        let _ = ReleaseDC(None, screen_dc);
        result
    }
}

unsafe fn query_window_hicon(hwnd: HWND) -> HICON {
    unsafe {
        for kind in [ICON_BIG as usize, ICON_SMALL2 as usize] {
            let mut res: usize = 0;
            let sent = SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(kind),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                120,
                Some(&mut res),
            );
            if sent.0 != 0 && res != 0 {
                return HICON(res as *mut _);
            }
        }
        for idx in [GCLP_HICON, GCLP_HICONSM] {
            let ptr = GetClassLongPtrW(hwnd, idx);
            if ptr != 0 {
                return HICON(ptr as *mut _);
            }
        }
        HICON::default()
    }
}

unsafe fn exe_hicon(exe_path: &str) -> Option<HICON> {
    unsafe {
        let mut sfi = SHFILEINFOW::default();
        let wide = HSTRING::from(exe_path);
        let r = SHGetFileInfoW(
            &wide,
            Default::default(),
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if r != 0 && !sfi.hIcon.is_invalid() {
            Some(sfi.hIcon)
        } else {
            None
        }
    }
}

unsafe fn render_hicon_to_rgba(hicon: HICON, size: i32) -> Option<Vec<u8>> {
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return None;
        }
        let dc = CreateCompatibleDC(Some(screen_dc));
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let out = if let Ok(dib) = dib
            && !dc.is_invalid()
            && !bits.is_null()
        {
            let pixel_bytes = (size * size * 4) as usize;
            std::ptr::write_bytes(bits as *mut u8, 0, pixel_bytes);
            let old = SelectObject(dc, HGDIOBJ(dib.0));
            let drawn = DrawIconEx(dc, 0, 0, hicon, size, size, 0, None, DI_NORMAL).is_ok();
            let rgba_res = if drawn {
                let raw = std::slice::from_raw_parts(bits as *const u8, pixel_bytes);
                let mut rgba = vec![0u8; pixel_bytes];
                let mut any_alpha = false;
                let mut any_color = false;
                for (src, dst) in raw.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                    dst[3] = src[3];
                    if src[3] != 0 {
                        any_alpha = true;
                    }
                    if src[0] != 0 || src[1] != 0 || src[2] != 0 {
                        any_color = true;
                    }
                }
                if !any_alpha && any_color {
                    if !apply_icon_mask_alpha(hicon, size, &mut rgba) {
                        for px in rgba.chunks_exact_mut(4) {
                            if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                                px[3] = 255;
                            }
                        }
                    }
                }
                if any_alpha || any_color { Some(rgba) } else { None }
            } else {
                None
            };
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(dib.0));
            rgba_res
        } else {
            None
        };
        if !dc.is_invalid() {
            let _ = DeleteDC(dc);
        }
        let _ = ReleaseDC(None, screen_dc);
        out
    }
}

unsafe fn apply_icon_mask_alpha(hicon: HICON, size: i32, rgba: &mut [u8]) -> bool {
    unsafe {
        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii).is_err() {
            return false;
        }
        let mut ok = false;
        if !ii.hbmMask.is_invalid() {
            let mut bm = BITMAP::default();
            if GetObjectW(HGDIOBJ(ii.hbmMask.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) > 0
                && bm.bmWidth > 0
                && bm.bmHeight > 0
            {
                let screen_dc = GetDC(None);
                let src_dc = CreateCompatibleDC(Some(screen_dc));
                let dst_dc = CreateCompatibleDC(Some(screen_dc));
                let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: size,
                        biHeight: -size,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                if let Ok(dib) = CreateDIBSection(Some(dst_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                    && !bits.is_null()
                {
                    let old_s = SelectObject(src_dc, HGDIOBJ(ii.hbmMask.0));
                    let old_d = SelectObject(dst_dc, HGDIOBJ(dib.0));
                    let mask_h = if ii.hbmColor.is_invalid() { bm.bmHeight / 2 } else { bm.bmHeight };
                    if StretchBlt(dst_dc, 0, 0, size, size, Some(src_dc), 0, 0, bm.bmWidth, mask_h.max(1), SRCCOPY).as_bool() {
                        let mraw = std::slice::from_raw_parts(bits as *const u8, (size * size * 4) as usize);
                        for (m, px) in mraw.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                            px[3] = if m[0] < 128 { 255 } else { 0 };
                        }
                        ok = true;
                    }
                    SelectObject(dst_dc, old_d);
                    SelectObject(src_dc, old_s);
                    let _ = DeleteObject(HGDIOBJ(dib.0));
                }
                let _ = DeleteDC(dst_dc);
                let _ = DeleteDC(src_dc);
                let _ = ReleaseDC(None, screen_dc);
            }
        }
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
        ok
    }
}

fn encode_rgba_png(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(8192);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

fn encode_bgra_as_rgb_png(bgra: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let mut non_black = 0usize;
    for px in bgra.chunks_exact(4) {
        let (b, g, r) = (px[0], px[1], px[2]);
        if (r | g | b) != 0 {
            non_black += 1;
        }
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    if non_black < 16 {
        return None;
    }
    let mut out = Vec::with_capacity(24 * 1024);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.set_filter(png::FilterType::Sub);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&rgb).ok()?;
    }
    Some(out)
}

fn inspect_window(hwnd: HWND, monitors: &[MonitorDesc]) -> Option<WindowDesc> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let is_tool = (ex_style & WS_EX_TOOLWINDOW.0) != 0;
        let is_app = (ex_style & WS_EX_APPWINDOW.0) != 0;
        if is_tool && !is_app {
            return None;
        }
        if !is_app {
            if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
                if !owner.is_invalid() {
                    return None;
                }
            }
        }

        let mut cloaked: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            size_of::<u32>() as u32,
        );
        if hr.is_ok() && cloaked != 0 {
            return None;
        }

        let title = read_window_title(hwnd);
        if title.is_empty() {
            return None;
        }

        let class_name = read_class_name(hwnd);
        if is_shell_noise_class(&class_name) {
            return None;
        }

        let minimized = IsIconic(hwnd).as_bool();
        let rect = window_rect(hwnd, minimized)?;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if !minimized && (width < 60 || height < 40) {
            return None;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        let mut app_key = process_app_key(pid).unwrap_or_default();
        if app_key == "applicationframehost" {
            if let Some(real_pid) = find_uwp_child_pid(hwnd, pid) {
                if let Some(real_key) = process_app_key(real_pid) {
                    app_key = real_key;
                }
            }
        }
        if is_shell_noise_app(&app_key, &title) {
            return None;
        }
        if app_key.is_empty() {
            app_key = "app".into();
        }

        let app_name = friendly_app_name(&app_key);
        let monitor = monitor_for_rect(&rect, monitors);
        let (rx, ry, rw, rh) = monitors
            .get(monitor as usize)
            .map(|m| normalize_rect_on_monitor((rect.left, rect.top, rect.right, rect.bottom), m))
            .unwrap_or((0, 0, 0, 0));

        Some(WindowDesc {
            hwnd: hwnd.0 as usize as u64,
            app_key,
            app_name,
            title,
            monitor,
            minimized,
            rx,
            ry,
            rw,
            rh,
        })
    }
}

fn read_window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len as usize).min(256) + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize]).trim().to_string()
    }
}

fn read_class_name(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn is_shell_noise_class(cls: &str) -> bool {
    matches!(
        cls,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Windows.UI.Core.CoreWindow"
            | "ForegroundStaging"
            | "ApplicationManager_DesktopShellWindow"
            | "TaskListThumbnailWnd"
            | "NotifyIconOverflowWindow"
            | "TopLevelWindowForOverflowXamlIsland"
    )
}

fn is_shell_noise_app(app_key: &str, title: &str) -> bool {
    matches!(
        app_key,
        "textinputhost"
            | "searchhost"
            | "shellexperiencehost"
            | "startmenuexperiencehost"
            | "lockapp"
            | "nvidia overlay"
            | "gamebar"
    ) || title == "Program Manager"
        || title == "Windows Input Experience"
}

fn window_rect(hwnd: HWND, minimized: bool) -> Option<RECT> {
    unsafe {
        if minimized {
            let mut wp = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            if GetWindowPlacement(hwnd, &mut wp).is_ok() {
                return Some(wp.rcNormalPosition);
            }
        }
        let mut ext = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut ext as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_ok()
            && ext.right > ext.left
            && ext.bottom > ext.top
        {
            return Some(ext);
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_ok() {
            return Some(r);
        }
        None
    }
}

pub(crate) fn normalize_rect_on_monitor(
    rect: (i32, i32, i32, i32),
    mon: &MonitorDesc,
) -> (u16, u16, u16, u16) {
    if mon.width == 0 || mon.height == 0 {
        return (0, 0, 0, 0);
    }
    let (left, top, right, bottom) = rect;
    let mon_left = mon.x as i64;
    let mon_top = mon.y as i64;
    let mon_right = mon_left + mon.width as i64;
    let mon_bottom = mon_top + mon.height as i64;

    let cl = (left as i64).clamp(mon_left, mon_right);
    let ct = (top as i64).clamp(mon_top, mon_bottom);
    let cr = (right as i64).clamp(mon_left, mon_right);
    let cb = (bottom as i64).clamp(mon_top, mon_bottom);
    if cr <= cl || cb <= ct {
        return (0, 0, 0, 0);
    }

    let mw = mon.width as f64;
    let mh = mon.height as f64;
    let norm = |num: i64, den: f64| -> u16 {
        ((num as f64 / den) * 65535.0).round().clamp(0.0, 65535.0) as u16
    };
    (
        norm(cl - mon_left, mw),
        norm(ct - mon_top, mh),
        norm(cr - cl, mw),
        norm(cb - ct, mh),
    )
}

fn monitor_for_rect(rect: &RECT, monitors: &[MonitorDesc]) -> u8 {
    if monitors.is_empty() {
        return 0;
    }
    let cx = ((rect.left as i64 + rect.right as i64) / 2) as i32;
    let cy = ((rect.top as i64 + rect.bottom as i64) / 2) as i32;
    for (idx, m) in monitors.iter().enumerate() {
        let right = m.x.saturating_add(m.width as i32);
        let bottom = m.y.saturating_add(m.height as i32);
        if cx >= m.x && cx < right && cy >= m.y && cy < bottom {
            return idx.min(255) as u8;
        }
    }
    let mut best_idx = monitors.iter().position(|m| m.primary).unwrap_or(0);
    let mut best_area = 0i64;
    for (idx, m) in monitors.iter().enumerate() {
        let left = rect.left.max(m.x);
        let top = rect.top.max(m.y);
        let right = rect.right.min(m.x.saturating_add(m.width as i32));
        let bottom = rect.bottom.min(m.y.saturating_add(m.height as i32));
        if right > left && bottom > top {
            let area = (right - left) as i64 * (bottom - top) as i64;
            if area > best_area {
                best_area = area;
                best_idx = idx;
            }
        }
    }
    best_idx.min(255) as u8
}

fn find_uwp_child_pid(hwnd: HWND, host_pid: u32) -> Option<u32> {
    struct ChildCtx {
        host_pid: u32,
        found: Option<u32>,
    }
    let mut ctx = ChildCtx { host_pid, found: None };
    unsafe extern "system" fn child_proc(child: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut ChildCtx) };
        let mut cpid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(child, Some(&mut cpid));
        }
        if cpid != 0 && cpid != ctx.host_pid {
            ctx.found = Some(cpid);
            return BOOL(0);
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(child_proc), LPARAM(&mut ctx as *mut ChildCtx as isize));
    }
    ctx.found
}

fn process_exe_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn real_window_exe_path(hwnd: HWND) -> Option<String> {
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return None;
    }
    let path = process_exe_path(pid)?;
    let stem = Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if stem == "applicationframehost" {
        if let Some(real_pid) = find_uwp_child_pid(hwnd, pid) {
            if let Some(real_path) = process_exe_path(real_pid) {
                return Some(real_path);
            }
        }
    }
    Some(path)
}

fn process_app_key(pid: u32) -> Option<String> {
    let full = process_exe_path(pid)?;
    let stem = Path::new(&full).file_stem()?.to_string_lossy().to_ascii_lowercase();
    if stem.is_empty() { None } else { Some(stem) }
}

fn friendly_app_name(app_key: &str) -> String {
    let known = match app_key {
        "code" | "code - insiders" => Some("VS Code"),
        "cursor" => Some("Cursor"),
        "chrome" => Some("Chrome"),
        "msedge" => Some("Edge"),
        "firefox" => Some("Firefox"),
        "brave" => Some("Brave"),
        "wechat" | "weixin" => Some("微信"),
        "qq" | "qqsc" => Some("QQ"),
        "telegram" => Some("Telegram"),
        "discord" => Some("Discord"),
        "slack" => Some("Slack"),
        "feishu" | "lark" => Some("飞书"),
        "dingtalk" => Some("钉钉"),
        "onenote" => Some("OneNote"),
        "notion" => Some("Notion"),
        "obsidian" => Some("Obsidian"),
        "windowsterminal" | "wt" => Some("终端"),
        "pwsh" | "powershell" => Some("PowerShell"),
        "cmd" => Some("命令提示符"),
        "explorer" => Some("文件"),
        "cloudmusic" => Some("网易云音乐"),
        "qqmusic" => Some("QQ 音乐"),
        "spotify" => Some("Spotify"),
        "winword" => Some("Word"),
        "excel" => Some("Excel"),
        "powerpnt" => Some("PowerPoint"),
        "notepad" => Some("记事本"),
        "notepad++" => Some("Notepad++"),
        "steam" | "steamwebhelper" => Some("Steam"),
        "remote-control" => Some("远程控制"),
        "systemsettings" => Some("设置"),
        "calculatorapp" => Some("计算器"),
        "mspaint" => Some("画图"),
        "photos" => Some("照片"),
        "taskmgr" => Some("任务管理器"),
        "v2rayn" => Some("v2rayN"),
        _ => None,
    };
    if let Some(name) = known {
        return name.to_string();
    }
    let mut chars = app_key.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => "应用".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_window_center_to_correct_monitor() {
        let mons = vec![
            MonitorDesc { name: "L".into(), x: -1920, y: 0, width: 1920, height: 1080, primary: false, rotation: 0 },
            MonitorDesc { name: "R".into(), x: 0, y: 0, width: 2560, height: 1440, primary: true, rotation: 0 },
        ];
        assert_eq!(monitor_for_rect(&RECT { left: -1200, top: 100, right: -200, bottom: 900 }, &mons), 0);
        assert_eq!(monitor_for_rect(&RECT { left: 200, top: 100, right: 1600, bottom: 1000 }, &mons), 1);
    }

    #[test]
    fn friendly_names_match_common_apps() {
        assert_eq!(friendly_app_name("code"), "VS Code");
        assert_eq!(friendly_app_name("wechat"), "微信");
        assert_eq!(friendly_app_name("windowsterminal"), "终端");
        assert_eq!(friendly_app_name("foobar"), "Foobar");
    }

    #[test]
    fn normalizes_window_bounds_on_primary_and_offset_monitors() {
        let primary = MonitorDesc {
            name: "Primary".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            primary: true,
            rotation: 0,
        };
        assert_eq!(
            normalize_rect_on_monitor((0, 0, 2560, 1440), &primary),
            (0, 0, 65535, 65535)
        );
        assert_eq!(
            normalize_rect_on_monitor((640, 360, 1920, 1080), &primary),
            (16384, 16384, 32768, 32768)
        );

        let secondary = MonitorDesc {
            name: "Secondary".into(),
            x: 2560,
            y: -666,
            width: 1920,
            height: 1080,
            primary: false,
            rotation: 0,
        };
        assert_eq!(
            normalize_rect_on_monitor((2560, -666, 3520, -126), &secondary),
            (0, 0, 32768, 32768)
        );

        // Partially off-screen window gets clamped to monitor bounds.
        assert_eq!(
            normalize_rect_on_monitor((2000, -800, 5000, 100), &secondary),
            (0, 0, 65535, ((766.0 / 1080.0) * 65535.0_f64).round() as u16)
        );

        // Completely outside or degenerate returns zeros.
        assert_eq!(normalize_rect_on_monitor((0, 0, 500, 500), &secondary), (0, 0, 0, 0));
        assert_eq!(normalize_rect_on_monitor((100, 100, 100, 200), &primary), (0, 0, 0, 0));
    }
}
