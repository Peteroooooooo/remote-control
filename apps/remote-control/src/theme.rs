//! The look all windows share: one palette (0xRRGGBB) for the egui lobby and
//! the GDI windows, and the native frame coloured to match it, so the title
//! bar and the content read as one surface. The frame keeps everything
//! Windows does (snap layouts, resizing, the caption buttons). Caption and
//! border colours need Windows 11; on Windows 10 those calls fail and the
//! frame stays the system's.

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        Graphics::Dwm::{
            DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
            DWMWCP_ROUND, DWMWINDOWATTRIBUTE, DwmSetWindowAttribute,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
            WindowsAndMessaging::{
                HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, SM_CXICON,
                SM_CXSMICON, SM_CYICON, SM_CYSMICON, SendMessageW, WM_SETICON,
            },
        },
    },
    core::PCWSTR,
};

use crate::gdi::rgb;

/// Title bars, the lobby's side pane, the confirmation popup.
pub const CHROME: u32 = 0x1b1c20;
/// The lobby's content area, one step up from the chrome.
pub const LAYER: u32 = 0x222428;
/// Device rows on the content area.
pub const CARD: u32 = 0x2a2c31;
/// Menus and dialogs above everything else.
pub const RAISED: u32 = 0x2d2f34;
/// The window border.
pub const BORDER: u32 = 0x313338;
pub const TEXT: u32 = 0xe6e7ea;
pub const DIM: u32 = 0x8f939b;
pub const ACCENT: u32 = 0x3b7fe0;
pub const ACCENT_HOVER: u32 = 0x4b8be6;
pub const ACCENT_PRESSED: u32 = 0x3471c9;
/// Secondary buttons on the chrome, with their edge: what the lobby's
/// buttons (light veils) come out as there.
pub const BUTTON: u32 = 0x292a2e;
pub const BUTTON_HOVER: u32 = 0x343539;
pub const BUTTON_EDGE: u32 = 0x36373b;

/// Colours the native frame of a top-level window: dark caption buttons,
/// the caption in `caption` (the surface right below it), a quiet border,
/// round corners. Call it before the window is first shown, or the light
/// frame flashes.
pub fn frame(hwnd: HWND, caption: u32) {
    set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &1i32); // TRUE
    set(hwnd, DWMWA_CAPTION_COLOR, &rgb(caption));
    set(hwnd, DWMWA_BORDER_COLOR, &rgb(BORDER));
    set(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);
}

/// The exe's icon resource (see build.rs).
const ICON_ID: PCWSTR = PCWSTR(1 as _);

/// The app's icon for a window class; Windows takes the small one from the
/// same resource. A null icon if it is missing, so the window still opens.
pub fn icon() -> HICON {
    unsafe { LoadIconW(GetModuleHandleW(None).ok().map(Into::into), ICON_ID).unwrap_or_default() }
}

/// Gives a window whose class is not ours (the lobby's) the app's icon, each
/// size drawn for its place instead of scaled from one picture.
pub fn set_icon(hwnd: HWND) {
    unsafe {
        let inst = GetModuleHandleW(None).ok().map(Into::into);
        let dpi = GetDpiForWindow(hwnd);
        for (which, cx, cy) in [(ICON_BIG, SM_CXICON, SM_CYICON), (ICON_SMALL, SM_CXSMICON, SM_CYSMICON)] {
            let size = (GetSystemMetricsForDpi(cx, dpi), GetSystemMetricsForDpi(cy, dpi));
            if let Ok(h) = LoadImageW(inst, ICON_ID, IMAGE_ICON, size.0, size.1, LR_DEFAULTCOLOR) {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(h.0 as isize)));
            }
        }
    }
}

fn set<T>(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: &T) {
    // Unknown attributes fail on older Windows; the frame then stays as it was.
    let _ = unsafe { DwmSetWindowAttribute(hwnd, attr, value as *const T as *const _, size_of::<T>() as u32) };
}
