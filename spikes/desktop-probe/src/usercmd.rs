//! Small commands the self test runs inside the user's session (started by the
//! service with the user's normal token), plus the elevated marker.

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{COLOR_HIGHLIGHT, HBRUSH},
        System::{LibraryLoader::GetModuleHandleW, Shutdown::LockWorkStation},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Shell::{IsUserAnAdmin, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
                GetSystemMetrics, IDC_ARROW, KillTimer, LoadCursorW, MSG, PostQuitMessage,
                RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SetTimer, WM_DESTROY,
                WM_LBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
                WS_VISIBLE,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};

use crate::log;

pub fn marker_path() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::DATA_DIR).join("uac-marker.txt")
}

/// Shows a small topmost window in the middle of the primary screen. When it is
/// clicked (the self test clicks it through the helper, like a user pressing a
/// button), it asks Windows to start `desktop-probe mark` elevated.
///
/// The click matters: Windows only brings the UAC prompt to the front when the
/// program asking for elevation owns the foreground; otherwise the prompt just
/// flashes in the taskbar. After 20 s without a click it asks anyway.
pub fn trigger_uac() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let inst = GetModuleHandleW(None)?;
        let class = w!("desktop-probe-uac");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hbrBackground: HBRUSH((COLOR_HIGHLIGHT.0 + 1) as _),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let (w, h) = (320, 160);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("desktop-probe UAC test"),
            WS_POPUP | WS_VISIBLE,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        SetTimer(Some(hwnd), 1, 20_000, None);
        log("trigger-uac: window shown, waiting for a click");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONUP | WM_TIMER => {
            unsafe {
                let _ = KillTimer(Some(hwnd), 1);
            }
            let how = if msg == WM_TIMER { "timeout" } else { "click" };
            let r = elevate(hwnd);
            log(&format!("trigger-uac: after {how}, ShellExecuteExW runas -> {r:?}"));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn elevate(owner: HWND) -> Result<()> {
    let exe = HSTRING::from(std::env::current_exe()?.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        hwnd: owner,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: w!("mark"),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.context("ShellExecuteExW runas")
}

/// Runs elevated after "Yes" was clicked: leaves proof that elevation happened.
pub fn mark() -> Result<()> {
    let admin = unsafe { IsUserAnAdmin() }.as_bool();
    std::fs::write(marker_path(), format!("elevated={}\n", admin as u8)).context("write marker")?;
    log(&format!("mark: elevated={admin}"));
    Ok(())
}

pub fn lock() -> Result<()> {
    let r = unsafe { LockWorkStation() };
    log(&format!("lock: LockWorkStation -> {r:?}"));
    r.context("LockWorkStation")
}
