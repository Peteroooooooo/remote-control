//! The lobby's icon in the taskbar's notification area, while it runs: a
//! click opens the window, the menu opens it or quits. Closing the window
//! only hides it. A second lobby started asks this one to show its window.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicIsize, Ordering},
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics, HICON, IMAGE_ICON, IsIconic,
                IsWindowVisible,
                LR_DEFAULTCOLOR, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, RegisterClassW,
                RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow,
                ShowWindow, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE,
                WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
            },
        },
    },
    core::{BOOL, HSTRING, PCWSTR, w},
};

const CLASS: PCWSTR = w!("remote-control-tray");
/// From the icon (`uCallbackMessage`).
const WM_TRAY: u32 = WM_APP + 1;
/// From a second lobby: show the window.
const WM_OPEN: u32 = WM_APP + 2;
const CMD_OPEN: usize = 1;
const CMD_QUIT: usize = 2;

/// The lobby window, once it exists.
static LOBBY: AtomicIsize = AtomicIsize::new(0);
/// The icon's window, once it is up.
static ICON: AtomicIsize = AtomicIsize::new(0);
static TOLD: AtomicBool = AtomicBool::new(false);

struct Handlers {
    shown: Box<dyn Fn() + Send + Sync>,
    quit: Box<dyn Fn() + Send + Sync>,
}

static HANDLERS: OnceLock<Handlers> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

pub fn set_window(hwnd: HWND) {
    LOBBY.store(hwnd.0 as isize, Ordering::SeqCst);
}

fn window() -> Option<HWND> {
    let h = LOBBY.load(Ordering::SeqCst);
    (h != 0).then_some(HWND(h as *mut _))
}

/// Started in the tray: eframe shows the window once it has drawn the first
/// frame anyway. Until then it is cloaked (drawn, never seen), then hidden.
static START_HIDDEN: AtomicBool = AtomicBool::new(false);

fn cloak(h: HWND, on: bool) {
    let v: BOOL = on.into();
    let _ = unsafe { DwmSetWindowAttribute(h, DWMWA_CLOAK, &v as *const BOOL as *const _, size_of::<BOOL>() as u32) };
}

/// The window starts in the tray: call it before the window is first shown.
pub fn start_hidden(hwnd: HWND) {
    cloak(hwnd, true);
    START_HIDDEN.store(true, Ordering::SeqCst);
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let h = HWND(raw as *mut _);
        for _ in 0..500 {
            if !START_HIDDEN.load(Ordering::SeqCst) || unsafe { IsWindowVisible(h) }.as_bool() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Not when the icon has opened the window meanwhile.
        if START_HIDDEN.swap(false, Ordering::SeqCst) {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
            cloak(h, false);
        }
    });
}

/// Shows the lobby window in front, and tells the backend.
pub fn show() {
    if START_HIDDEN.swap(false, Ordering::SeqCst)
        && let Some(h) = window()
    {
        cloak(h, false);
    }
    if let Some(h) = window() {
        unsafe {
            let _ = ShowWindow(h, if IsIconic(h).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(h);
        }
    }
    if let Some(hs) = HANDLERS.get() {
        (hs.shown)();
    }
}

/// Hides the lobby window (the icon stays). The first time, a notification
/// says where it went.
pub fn hide() {
    if let Some(h) = window() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
    let icon = ICON.load(Ordering::SeqCst);
    if icon != 0 && !TOLD.swap(true, Ordering::SeqCst) {
        let mut nid = icon_data(HWND(icon as *mut _));
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        let copy = |dst: &mut [u16], s: &str| {
            let v: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
            dst[..v.len()].copy_from_slice(&v);
        };
        copy(&mut nid.szInfoTitle, "remote control 还在运行");
        copy(&mut nid.szInfo, "点这里的图标打开窗口，右键可以退出");
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

/// Asks the lobby already running to show its window; false if there is none.
pub fn open_running() -> bool {
    unsafe {
        let Ok(h) = FindWindowW(CLASS, None) else { return false };
        // The running lobby may come to the front: this process was just started by the user.
        let _ = AllowSetForegroundWindow(u32::MAX);
        PostMessageW(Some(h), WM_OPEN, WPARAM(0), LPARAM(0)).is_ok()
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    unsafe {
        // Id 1: the exe's icon (build.rs), at the size the tray draws.
        if let Ok(m) = GetModuleHandleW(None)
            && let Ok(icon) = LoadImageW(
                Some(m.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            )
        {
            nid.hIcon = HICON(icon.0);
        }
    }
    let tip: Vec<u16> = "remote control".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

fn menu(hwnd: HWND) {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(m, MF_STRING, CMD_OPEN, w!("打开"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, CMD_QUIT, w!("退出"));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        // Without this the menu stays open when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => match (lp.0 & 0xffff) as u32 {
            WM_LBUTTONUP => show(),
            WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd),
            _ => {}
        },
        WM_OPEN => show(),
        WM_COMMAND if wp.0 & 0xffff == CMD_OPEN => show(),
        WM_COMMAND if wp.0 & 0xffff == CMD_QUIT => unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            if let Some(hs) = HANDLERS.get() {
                (hs.quit)();
            }
        },
        // Explorer restarted: the icon is gone from the new taskbar.
        m if Some(&m) == TASKBAR_CREATED.get() => unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
        },
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Puts the icon up, on a thread of its own. `shown`: the window was shown
/// from the icon (or by a second lobby); `quit`: 退出 in the menu.
pub fn spawn(shown: impl Fn() + Send + Sync + 'static, quit: impl Fn() + Send + Sync + 'static) {
    let _ = HANDLERS.set(Handlers { shown: Box::new(shown), quit: Box::new(quit) });
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(|| unsafe {
            let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(&HSTRING::from("TaskbarCreated")));
            let Ok(module) = GetModuleHandleW(None) else { return };
            let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: CLASS, ..Default::default() };
            RegisterClassW(&class);
            // A hidden top-level window: message-only windows miss TaskbarCreated.
            let Ok(hwnd) =
                CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!("remote control tray"), WS_POPUP, 0, 0, 0, 0, None, None, Some(module.into()), None)
            else {
                return;
            };
            let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
            ICON.store(hwnd.0 as isize, Ordering::SeqCst);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("tray thread");
}
