//! The viewer's session window: a title bar drawn by toolbar.rs (the window
//! has no system caption; it gives the picture's area to a child window) and
//! below it the picture, a child window the render thread draws into (see
//! present.rs). Mouse events on the picture and keyboard events in the
//! window go to the host.
//! While the window has focus and input is captured, a low-level keyboard
//! hook sends system keys (Win, Alt+Tab, Ctrl+Esc) to the host too.
//! Ctrl+Alt+Shift+Q closes the window (Alt+F4 goes to the host);
//! Ctrl+Alt+Shift+Z releases keyboard and mouse until the picture is clicked;
//! Ctrl+Alt+Shift+Left / Right / 1..9 switch the host's display;
//! Ctrl+Alt+End sends Ctrl+Alt+Del (needs the host's service).

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, LazyLock, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
};

use anyhow::{Result, bail};
use tokio::sync::mpsc::UnboundedSender;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BLACK_BRUSH, GetMonitorInfoW, GetStockObject, HBRUSH, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromWindow, ScreenToClient,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi},
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetFocus, GetKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, ReleaseCapture,
                SetCapture, SetFocus, VK_CONTROL, VK_END, VK_MENU, VK_SHIFT,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, w},
};

use crate::{
    cursor::{self, CursorImage},
    keys::{self, KeyRouter, Mods, Route},
    log,
    proto::Msg,
    theme,
    vp::letterbox,
};

pub struct WindowState {
    pub ctrl: UnboundedSender<Msg>,
    /// Size of the streamed picture, for mapping the pointer.
    pub stream: Mutex<(u32, u32)>,
    pub closed: AtomicBool,
    input: bool,
    /// Keyboard and mouse go to the host (until Ctrl+Alt+Shift+Z).
    captured: AtomicBool,
    /// The keyboard hook also takes injected events marked `HOOK_TEST_MARK`, and logs (for tests).
    hook_test: bool,
    title: Mutex<String>,
    last_pos: Mutex<(u16, u16)>,
}

/// `dwExtraInfo` of injected key events that `--hook-test` treats as real ones.
pub const HOOK_TEST_MARK: usize = 0x5243_4B54;

impl WindowState {
    pub fn new(ctrl: UnboundedSender<Msg>, stream: (u32, u32), input: bool, hook_test: bool) -> Arc<WindowState> {
        Arc::new(WindowState {
            ctrl,
            stream: Mutex::new(stream),
            closed: AtomicBool::new(false),
            input,
            captured: AtomicBool::new(input),
            hook_test,
            title: Mutex::default(),
            last_pos: Mutex::new((0, 0)),
        })
    }
}

static STATE: OnceLock<Arc<WindowState>> = OnceLock::new();
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The picture: a child window below the title bar (all of the window in full screen).
static PICTURE: AtomicIsize = AtomicIsize::new(0);
/// Where `--cursor-dump` saves a picture of each cursor built (for tests).
static CURSOR_DUMP: OnceLock<PathBuf> = OnceLock::new();

/// Posted by the keyboard hook after Ctrl+Alt+Shift+Z.
const WM_APP_RELEASE: u32 = WM_APP + 1;
/// The host's cursor changed.
const WM_APP_CURSOR: u32 = WM_APP + 2;
/// More cursor images than any desktop uses; beyond this new ones are not kept.
const MAX_CURSORS: usize = 512;

enum Shown {
    /// Nothing heard from the host yet: the local arrow.
    Local,
    Hidden,
    Remote(u32),
}

/// The host's cursor images by id, filled from the network thread.
static CURSORS: LazyLock<Mutex<(HashMap<u32, CursorImage>, Shown)>> =
    LazyLock::new(|| Mutex::new((HashMap::new(), Shown::Local)));

// The hook runs on the window's thread, inside its message loop.
thread_local! {
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static ROUTER: RefCell<KeyRouter> = RefCell::default();
    /// Buttons whose press recaptured input; their release is not sent either.
    static SKIP_UP: Cell<u8> = const { Cell::new(0) };
    /// Cursors built from the host's images, by id.
    static BUILT: RefCell<HashMap<u32, HCURSOR>> = RefCell::default();
}

/// The host's displays: (streamed one, how many), from its latest list.
static MONITORS: Mutex<(usize, usize)> = Mutex::new((0, 0));

pub fn set_cursor_dump(dir: PathBuf) {
    let _ = CURSOR_DUMP.set(dir);
}

/// The host sent its display list (any thread, also before the window exists).
pub fn set_monitors(current: usize, count: usize) {
    *MONITORS.lock().unwrap() = (current, count);
    let h = WINDOW.load(Ordering::Relaxed);
    if h != 0 {
        show_title(HWND(h as *mut _));
    }
}

/// Asks the host for another display (hotkeys and `--script`). Works in
/// watch-only mode too: it is not input for the host's desktop.
pub fn pick_monitor(key: keys::MonitorKey) {
    let (current, count) = *MONITORS.lock().unwrap();
    let index = match key {
        keys::MonitorKey::Pick(i) => i as usize,
        keys::MonitorKey::Prev => crate::monitors::cycle(current, count, -1),
        keys::MonitorKey::Next => crate::monitors::cycle(current, count, 1),
    };
    log!("asking for display {} of {count} ({key:?})", index + 1);
    if let Some(s) = STATE.get() {
        let _ = s.ctrl.send(Msg::SelectMonitor { index: index.min(255) as u8 });
    }
}

/// The stream changed size (another display, or the host's resolution
/// changed): map the pointer for the new size.
pub fn set_stream(size: (u32, u32)) {
    let Some(s) = STATE.get() else { return };
    *s.stream.lock().unwrap() = size;
    let h = PICTURE.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_CURSOR, WPARAM(0), LPARAM(0));
        }
    }
}

/// A cursor message from the host (any thread).
pub fn remote_cursor(m: Msg) {
    {
        let mut c = CURSORS.lock().unwrap();
        match m {
            Msg::CursorShape { id, image } => {
                log!(
                    "cursor {id}: {}x{} {}, hotspot ({}, {})",
                    image.width,
                    image.height,
                    if image.is_color() { "colour" } else { "monochrome" },
                    image.hot_x,
                    image.hot_y
                );
                if c.0.len() < MAX_CURSORS || c.0.contains_key(&id) {
                    c.0.insert(id, image);
                }
                c.1 = Shown::Remote(id);
            }
            Msg::CursorUse { id } => c.1 = Shown::Remote(id),
            Msg::CursorHidden => {
                if !matches!(c.1, Shown::Hidden) {
                    log!("cursor hidden");
                }
                c.1 = Shown::Hidden
            }
            _ => return,
        }
    }
    let h = PICTURE.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_CURSOR, WPARAM(0), LPARAM(0));
        }
    }
}

/// Opens the window with a picture of `client` pixels under the title bar;
/// returns the picture's window, which the render thread draws into.
pub fn create(state: Arc<WindowState>, title: &str, client: (u32, u32)) -> Result<HWND> {
    if STATE.set(state).is_err() {
        bail!("only one session window per process");
    }
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("remote-control-session");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: theme::icon(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let pic_class = w!("remote-control-picture");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(picture_proc),
            hInstance: inst.into(),
            lpszClassName: pic_class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
        let (w, h) = (client.0 as i32, client.1 as i32);
        let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
        AdjustWindowRectEx(&mut r, style, false, WINDOW_EX_STYLE::default())?;
        // The title bar is client area: no frame above it, only below.
        let caption = crate::toolbar::CAPTION * GetDpiForSystem().max(96) as i32 / 96;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            &HSTRING::from(title),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            caption + h + (r.bottom - h),
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
        let pic = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            pic_class,
            None,
            WS_CHILD | WS_VISIBLE,
            0,
            caption,
            w,
            h,
            Some(hwnd),
            None,
            Some(inst.into()),
            None,
        )?;
        PICTURE.store(pic.0 as isize, Ordering::Relaxed);
        // Coloured while still hidden, so the light frame never flashes.
        theme::frame(hwnd, theme::CHROME);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        crate::toolbar::create(hwnd);
        place_picture(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        set_title(hwnd, title);
        Ok(pic)
    }
}

/// The picture fills the window below the title bar, or all of it in full screen.
fn place_picture(hwnd: HWND) {
    let pic = picture();
    if pic.is_invalid() {
        return;
    }
    let mut c = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut c);
        let top = if fullscreen() { 0 } else { caption_px(hwnd) };
        let _ = MoveWindow(pic, 0, top, c.right, (c.bottom - top).max(1), true);
    }
}

fn caption_px(hwnd: HWND) -> i32 {
    crate::toolbar::CAPTION * unsafe { GetDpiForWindow(hwnd) }.max(96) as i32 / 96
}

/// The picture's window (null before and after).
pub fn picture() -> HWND {
    HWND(PICTURE.load(Ordering::Relaxed) as *mut _)
}

/// The host's name, as the title bar shows it.
pub fn name() -> String {
    STATE.get().map(|s| s.title.lock().unwrap().clone()).unwrap_or_default()
}

/// A client size that fits 85% of the screen's work area with the stream's aspect.
pub fn initial_client_size(stream: (u32, u32)) -> (u32, u32) {
    let mut work = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut _), Default::default());
    }
    let avail = (((work.right - work.left) as f64 * 0.85) as u32, ((work.bottom - work.top) as f64 * 0.85) as u32);
    let r = letterbox(stream, avail);
    ((r.right - r.left).max(320) as u32, (r.bottom - r.top).max(180) as u32)
}

/// Runs the message loop until the window is closed.
pub fn run_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

/// Sets the title, with the capture state in front.
fn set_title(hwnd: HWND, title: &str) {
    if let Some(s) = STATE.get() {
        *s.title.lock().unwrap() = title.to_string();
    }
    show_title(hwnd);
}

/// The title (for the taskbar), and the title bar drawn again.
fn show_title(hwnd: HWND) {
    unsafe {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(name()));
    }
    crate::toolbar::refresh();
}

/// The host's displays: (streamed one, how many).
pub fn monitors() -> (usize, usize) {
    *MONITORS.lock().unwrap()
}

/// Keyboard and mouse may go to the host (not watching only).
pub fn input() -> bool {
    STATE.get().is_some_and(|s| s.input)
}

fn hwnd() -> HWND {
    HWND(WINDOW.load(Ordering::Relaxed) as *mut _)
}

/// Ctrl+Alt+Del on the host (needs its service).
pub fn send_sas() {
    send(Msg::ReleaseAll);
    ROUTER.with_borrow_mut(KeyRouter::clear);
    send(Msg::SendSas);
}

/// Keyboard and mouse back to this computer, or to the host again.
pub fn toggle_capture() {
    if captured() { release() } else { capture() }
}

/// Ends the session.
pub fn disconnect() {
    send(Msg::ReleaseAll);
    ROUTER.with_borrow_mut(KeyRouter::clear);
    close();
}

/// Where the window was before it filled its display.
static FULL: Mutex<Option<WINDOWPLACEMENT>> = Mutex::new(None);

pub fn fullscreen() -> bool {
    FULL.lock().unwrap().is_some()
}

/// The window fills its display without a frame, or gets its frame back.
pub fn toggle_fullscreen() {
    let hwnd = hwnd();
    let before = FULL.lock().unwrap().take();
    unsafe {
        match before {
            Some(wp) => {
                SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VISIBLE).0 as isize);
                let _ = SetWindowPlacement(hwnd, &wp);
                let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
                log!("full screen off");
            }
            None => {
                let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                let _ = GetWindowPlacement(hwnd, &mut wp);
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
                let r = mi.rcMonitor;
                *FULL.lock().unwrap() = Some(wp);
                SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_CLIPCHILDREN | WS_VISIBLE).0 as isize);
                let _ = SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED);
                log!("full screen on");
            }
        }
    }
    place_picture(hwnd);
    crate::toolbar::refresh();
}

/// Closes the session window (any thread).
pub fn close() {
    unsafe {
        let _ = PostMessageW(Some(hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

fn send(m: Msg) {
    if let Some(s) = STATE.get()
        && s.input
    {
        let _ = s.ctrl.send(m);
    }
}

fn down(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

/// Window position to stream position (0..=65535), clamped to the picture.
fn to_stream(hwnd: HWND, lparam: LPARAM) -> (u16, u16) {
    let x = (lparam.0 & 0xffff) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
    let mut c = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut c);
    }
    let stream = STATE.get().map(|s| *s.stream.lock().unwrap()).unwrap_or((1, 1));
    let r = letterbox(stream, (c.right.max(1) as u32, c.bottom.max(1) as u32));
    let norm = |v: i32, a: i32, b: i32| -> u16 {
        let span = (b - a - 1).max(1) as i64;
        ((v - a).clamp(0, span as i32) as i64 * 65535 / span) as u16
    };
    (norm(x, r.left, r.right), norm(y, r.top, r.bottom))
}

pub fn captured() -> bool {
    STATE.get().is_some_and(|s| s.input && s.captured.load(Ordering::Relaxed))
}

fn hook_on() -> bool {
    HOOK.get().is_some()
}

fn install_hook() {
    if hook_on() || !captured() {
        return;
    }
    let r = unsafe {
        GetModuleHandleW(None).and_then(|m| SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(m.into()), 0))
    };
    match r {
        Ok(h) => HOOK.set(Some(h)),
        Err(e) => log!("keyboard hook not installed, system keys stay local: {e}"),
    }
}

fn remove_hook() {
    if let Some(h) = HOOK.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

/// Input goes to the host again (a click on the picture).
fn capture() {
    let pic = picture();
    if let Some(s) = STATE.get() {
        s.captured.store(true, Ordering::Relaxed);
    }
    if unsafe { GetFocus() } == pic {
        install_hook();
    }
    log!("input captured");
    show_title(hwnd());
    refresh_cursor(pic);
}

/// Ctrl+Alt+Shift+Z: the host lets go of everything, the local system gets keys and mouse back.
fn release() {
    if let Some(s) = STATE.get() {
        s.captured.store(false, Ordering::Relaxed);
    }
    send(Msg::ReleaseAll);
    ROUTER.with_borrow_mut(KeyRouter::clear);
    remove_hook();
    unsafe {
        let _ = ReleaseCapture();
    }
    log!("input released");
    show_title(hwnd());
    refresh_cursor(picture());
}

/// The cursor to show over the picture: None for the local arrow, Some(None) for no cursor.
/// Shown at the size the host shows it, whatever the picture's scale (as RDP and Parsec do).
fn remote_hcursor() -> Option<Option<HCURSOR>> {
    let c = CURSORS.lock().unwrap();
    let id = match c.1 {
        Shown::Local => return None,
        Shown::Hidden => return Some(None),
        Shown::Remote(id) => id,
    };
    BUILT.with_borrow_mut(|built| {
        if let Some(h) = built.get(&id) {
            return Some(Some(*h));
        }
        let img = c.0.get(&id)?;
        match cursor::build(img) {
            Ok(h) => {
                if let Some(dir) = CURSOR_DUMP.get() {
                    match cursor::dump(h, img, &dir.join(format!("cursor-{id}.png"))) {
                        Ok(()) => log!("cursor {id} built at {}x{}, saved", img.width, img.height),
                        Err(e) => log!("cursor {id}: dump failed: {e:#}"),
                    }
                }
                built.insert(id, h);
                Some(Some(h))
            }
            Err(e) => {
                log!("cursor {id}: {e:#}");
                None
            }
        }
    })
}

/// Shows the current cursor now if the pointer is over the window, instead
/// of waiting for the next mouse move.
fn refresh_cursor(hwnd: HWND) {
    let mut p = Default::default();
    unsafe {
        if GetCursorPos(&mut p).is_ok() && WindowFromPoint(p) == hwnd {
            SendMessageW(hwnd, WM_SETCURSOR, Some(WPARAM(hwnd.0 as usize)), Some(LPARAM((WM_MOUSEMOVE << 16 | HTCLIENT) as isize)));
        }
    }
}

fn set_cursor(lparam: LPARAM) -> bool {
    // Released input means the pointer is the local one again; watching
    // only (no input) still shows the host's cursor.
    let local = STATE.get().is_some_and(|s| s.input) && !captured();
    if (lparam.0 & 0xffff) as u32 != HTCLIENT || local {
        return false;
    }
    match remote_hcursor() {
        Some(h) => {
            unsafe { SetCursor(h) };
            true
        }
        None => false,
    }
}

fn async_down(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) < 0 }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && hook_key(unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) }) {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// True to keep the event from the local system.
fn hook_key(kb: &KBDLLHOOKSTRUCT) -> bool {
    let hwnd = HWND(WINDOW.load(Ordering::Relaxed) as *mut _);
    let test = STATE.get().is_some_and(|s| s.hook_test) && kb.dwExtraInfo == HOOK_TEST_MARK;
    // Injected events include a host on this same computer replaying our own
    // keys; sending them again would loop.
    if (kb.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0 && !test)
        || !captured()
        || unsafe { GetForegroundWindow() } != hwnd
    {
        return false;
    }
    let down = kb.flags.0 & LLKHF_UP.0 == 0;
    let scan = match kb.scanCode {
        // Some keys (media keys) come without a scan code.
        0 => unsafe { MapVirtualKeyW(kb.vkCode, MAPVK_VK_TO_VSC_EX) as u16 },
        raw => keys::scan_code(raw, kb.flags.0 & LLKHF_EXTENDED.0 != 0),
    };
    // Keys the hook swallows never reach this state, so it holds only what
    // the local system saw.
    let local = Mods { ctrl: async_down(VK_CONTROL), alt: async_down(VK_MENU), shift: async_down(VK_SHIFT) };
    let route = ROUTER.with_borrow_mut(|r| r.route(kb.vkCode, scan, down, local));
    if STATE.get().is_some_and(|s| s.hook_test) {
        log!("hook: vk {:#04x} scan {scan:#06x} {} -> {route:?}", kb.vkCode, if down { "down" } else { "up" });
    }
    match route {
        Route::Local => false,
        Route::Remote { scan, down } => {
            send(Msg::Key { scan, down });
            true
        }
        Route::Drop => true,
        Route::Monitor(m) => {
            pick_monitor(m);
            true
        }
        Route::Quit => {
            send(Msg::ReleaseAll);
            ROUTER.with_borrow_mut(KeyRouter::clear);
            close();
            true
        }
        Route::Sas => {
            // Ctrl+Alt+Del itself never reaches the hook; Ctrl+Alt+End stands in for it.
            send(Msg::ReleaseAll);
            ROUTER.with_borrow_mut(KeyRouter::clear);
            send(Msg::SendSas);
            true
        }
        Route::Release => {
            // Stop routing at once; the hook itself is removed outside its own callback.
            if let Some(s) = STATE.get() {
                s.captured.store(false, Ordering::Relaxed);
            }
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_APP_RELEASE, WPARAM(0), LPARAM(0));
            }
            true
        }
    }
}

fn mouse_move(hwnd: HWND, lparam: LPARAM) {
    if !captured() {
        return;
    }
    let (x, y) = to_stream(hwnd, lparam);
    if let Some(s) = STATE.get() {
        let mut last = s.last_pos.lock().unwrap();
        if *last == (x, y) {
            return;
        }
        *last = (x, y);
    }
    send(Msg::MouseMove { x, y });
}

fn button(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    let x = ((wparam.0 >> 16) & 0xffff) as u16;
    let (b, pressed) = match msg {
        WM_LBUTTONDOWN => (0, true),
        WM_LBUTTONUP => (0, false),
        WM_RBUTTONDOWN => (1, true),
        WM_RBUTTONUP => (1, false),
        WM_MBUTTONDOWN => (2, true),
        WM_MBUTTONUP => (2, false),
        WM_XBUTTONDOWN => (if x == XBUTTON1 { 3 } else { 4 }, true),
        WM_XBUTTONUP => (if x == XBUTTON1 { 3 } else { 4 }, false),
        _ => return,
    };
    let bit = 1u8 << b;
    if !pressed && SKIP_UP.get() & bit != 0 {
        SKIP_UP.set(SKIP_UP.get() & !bit);
        return;
    }
    if !captured() {
        if pressed && STATE.get().is_some_and(|s| s.input) {
            SKIP_UP.set(SKIP_UP.get() | bit);
            capture();
        }
        return;
    }
    mouse_move(hwnd, lparam);
    unsafe {
        if pressed {
            SetCapture(hwnd);
        } else if wparam.0 & 0x73 == 0 {
            // No button left down (MK_LBUTTON | MK_RBUTTON | MK_MBUTTON | MK_XBUTTON1 | MK_XBUTTON2).
            let _ = ReleaseCapture();
        }
    }
    send(Msg::MouseButton { button: b, down: pressed });
}

/// Keys that reach the window: all of them when the hook is not installed,
/// none while it is (it swallows what it sends). False to let Windows handle the key.
fn key(wparam: WPARAM, lparam: LPARAM, pressed: bool) -> bool {
    let vk = wparam.0 as u32;
    let hotkey = pressed && down(VK_CONTROL) && down(VK_MENU) && down(VK_SHIFT);
    if hotkey && vk == 'Q' as u32 {
        send(Msg::ReleaseAll);
        unsafe {
            let _ = DestroyWindow(hwnd());
        }
        return true;
    }
    if hotkey && let Some(m) = keys::monitor_key(vk) {
        pick_monitor(m);
        return true;
    }
    if !captured() {
        return false;
    }
    if hook_on() {
        return true;
    }
    if hotkey && vk == 'Z' as u32 {
        release();
        return true;
    }
    if pressed && vk == VK_END.0 as u32 && down(VK_CONTROL) && down(VK_MENU) {
        // Ctrl+Alt+Del itself never reaches a window; Ctrl+Alt+End stands in for it.
        send(Msg::ReleaseAll);
        send(Msg::SendSas);
        return true;
    }
    let mut scan = ((lparam.0 >> 16) & 0xff) as u16;
    let extended = (lparam.0 >> 24) & 1 == 1;
    if scan == 0 {
        // Some keys (media keys) come without a scan code.
        let s = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX) } as u16;
        if s == 0 {
            return true;
        }
        scan = s;
    } else if extended {
        scan |= 0xE000;
    }
    send(Msg::Key { scan, down: pressed });
    true
}

unsafe extern "system" fn picture_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => mouse_move(hwnd, lparam),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP
        | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
                crate::toolbar::dismiss();
            }
            button(hwnd, msg, wparam, lparam);
            if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                return LRESULT(1);
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if captured() {
                let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16;
                send(Msg::Wheel { delta, horizontal: msg == WM_MOUSEHWHEEL });
            }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            if !key(wparam, lparam, matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN)) {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
        }
        WM_MOUSEACTIVATE => {
            // Clicking the picture focuses it, so keys and the hook follow.
            unsafe {
                let _ = SetFocus(Some(hwnd));
            }
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_SETFOCUS => install_hook(),
        WM_KILLFOCUS => {
            send(Msg::ReleaseAll);
            ROUTER.with_borrow_mut(KeyRouter::clear);
            remove_hook();
        }
        WM_SETCURSOR => {
            if !set_cursor(lparam) {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            return LRESULT(1);
        }
        WM_APP_CURSOR => {
            if CURSOR_DUMP.get().is_some() {
                let _ = remote_hcursor();
            }
            refresh_cursor(hwnd);
        }
        WM_ERASEBKGND => return LRESULT(1),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

/// The frame Windows keeps around the window (which a maximised window
/// pushes off the screen): none at the top, where the title bar is client area.
fn calc_size(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let p = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
        let (top, left) = (p.rgrc[0].top, p.rgrc[0].left);
        let r = DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam);
        p.rgrc[0].top = top;
        if IsZoomed(hwnd).as_bool() && !fullscreen() {
            p.rgrc[0].top += p.rgrc[0].left - left;
        }
        r
    }
}

fn hit_test(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let mut p = windows::Win32::Foundation::POINT { x: lparam.0 as i16 as i32, y: (lparam.0 >> 16) as i16 as i32 };
    unsafe {
        let def = DefWindowProcW(hwnd, WM_NCHITTEST, WPARAM(0), lparam);
        if def.0 as u32 != HTCLIENT || fullscreen() {
            return def;
        }
        let _ = ScreenToClient(hwnd, &mut p);
        let dpi = GetDpiForWindow(hwnd);
        let border = GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        if p.y < border && !IsZoomed(hwnd).as_bool() {
            let mut c = RECT::default();
            let _ = GetClientRect(hwnd, &mut c);
            let ht = if p.x < border * 2 {
                HTTOPLEFT
            } else if p.x >= c.right - border * 2 {
                HTTOPRIGHT
            } else {
                HTTOP
            };
            return LRESULT(ht as isize);
        }
        if p.y < caption_px(hwnd) {
            return LRESULT(crate::toolbar::caption_hit(p.x, p.y) as isize);
        }
        def
    }
}

fn invalidate_caption(hwnd: HWND) {
    let mut c = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut c);
        c.bottom = c.bottom.min(caption_px(hwnd));
        let _ = InvalidateRect(Some(hwnd), Some(&c), false);
    }
}

/// The window itself: its title bar, frame and size; the picture has the rest.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(r) = crate::toolbar::caption_mouse(hwnd, msg, wparam, lparam) {
        return r;
    }
    match msg {
        WM_NCCALCSIZE if wparam.0 != 0 => return calc_size(hwnd, wparam, lparam),
        WM_NCHITTEST => return hit_test(hwnd, lparam),
        WM_PAINT => crate::toolbar::paint_caption(hwnd),
        WM_ERASEBKGND => return LRESULT(1),
        WM_ACTIVATE => {
            invalidate_caption(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_SETFOCUS => unsafe {
            // Keys go to the picture.
            let _ = SetFocus(Some(picture()));
        },
        WM_APP_RELEASE => release(),
        WM_SIZE => {
            place_picture(hwnd);
            invalidate_caption(hwnd);
            crate::toolbar::follow();
            crate::quality::follow_window(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_MOVE => {
            crate::toolbar::follow();
            crate::quality::follow_window(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_DISPLAYCHANGE => {
            crate::quality::follow_window(hwnd);
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_DPICHANGED => unsafe {
            let r = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
        },
        WM_DESTROY => {
            remove_hook();
            WINDOW.store(0, Ordering::Relaxed);
            PICTURE.store(0, Ordering::Relaxed);
            BUILT.with_borrow_mut(|built| {
                for (_, h) in built.drain() {
                    unsafe {
                        let _ = DestroyCursor(h);
                    }
                }
            });
            if let Some(s) = STATE.get() {
                s.closed.store(true, Ordering::SeqCst);
            }
            unsafe { PostQuitMessage(0) };
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
