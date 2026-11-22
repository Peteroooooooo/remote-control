//! The popup on the host asking whether a device may control this computer
//! (`target-plan` 2). It runs on its own thread with its own message loop,
//! so the lobby and the service's helper can both show it; in the helper the
//! thread first joins the input desktop, where the user is. Enter does
//! nothing (a key typed by chance must not let anyone in); Esc and the close
//! button refuse; no answer within the countdown refuses too.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::oneshot;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::InvalidateRect,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
            Input::KeyboardAndMouse::{
                GetCapture, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, w},
};

use crate::{
    gdi::{self, Canvas, rect},
    i18n::{is_en, tr},
    log, theme,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Not in the device list: met with a connection code, or on the LAN.
    New { via_code: bool },
    Trusted,
}

/// Only allow or deny: the popup never changes what the device is to this
/// computer (`devices-and-relay.md` 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Answer {
    Allow,
    Deny,
    NoAnswer,
    /// Closed by the program (the viewer gave up).
    Cancelled,
}

pub struct Asking {
    hwnd: Arc<AtomicIsize>,
    cancelled: Arc<AtomicBool>,
    pub rx: oneshot::Receiver<Answer>,
}

impl Asking {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let h = self.hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                // wParam 1: closed by the program, not the user.
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, WPARAM(1), LPARAM(0));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Allow,
    Deny,
}

struct Popup {
    name: String,
    kind: Kind,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    left_s: u32,
    wait_s: u32,
    shown_at: Instant,
    tx: Option<oneshot::Sender<Answer>>,
}

/// Ignore accidental clicks on "允许" right after the popup appears.
const COOLDOWN: Duration = Duration::from_millis(300);

/// The client area in window units.
const WIDTH: i32 = 440;
const HEIGHT: i32 = 164;
/// The countdown bar.
const AMBER: windows::Win32::Foundation::COLORREF = gdi::rgb(0xd29922);

impl Popup {
    /// Where the buttons go, in window units: 拒绝, then 允许 on the right.
    fn layout(&self) -> [(Hit, RECT); 2] {
        let by = HEIGHT - 20 - 36;
        let allow = rect(WIDTH - 24 - 100, by, WIDTH - 24, by + 36);
        let deny = rect(allow.left - 10 - 92, by, allow.left - 10, by + 36);
        [(Hit::Deny, deny), (Hit::Allow, allow)]
    }

    fn hit(&self, hwnd: HWND, x: i32, y: i32) -> Option<Hit> {
        let px = gdi::scale(hwnd);
        self.layout().into_iter().find(|(_, r)| gdi::inside(&rect(px(r.left), px(r.top), px(r.right), px(r.bottom)), x, y)).map(|(h, _)| h)
    }
}

/// Shows the popup; the answer arrives on `Asking::rx`.
pub fn ask(name: &str, kind: Kind, wait: Duration) -> Asking {
    let hwnd = Arc::new(AtomicIsize::new(0));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (tx, rx) = oneshot::channel();
    let wait_s = wait.as_secs().max(1) as u32;
    let popup = Popup { name: name.to_string(), kind, hover: None, pressed: None, left_s: wait_s, wait_s, shown_at: Instant::now(), tx: Some(tx) };
    let (h, c) = (hwnd.clone(), cancelled.clone());
    let r = std::thread::Builder::new().name("consent".into()).spawn(move || {
        if let Err(e) = crate::desktop::follow() {
            log!("confirmation popup: {e:#}");
        }
        if let Err(e) = run(popup, &h, &c) {
            log!("confirmation popup could not open: {e:#}");
        }
    });
    if let Err(e) = r {
        log!("confirmation popup thread: {e}");
    }
    Asking { hwnd, cancelled, rx }
}

const TIMER: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

fn run(mut popup: Popup, hwnd_out: &AtomicIsize, cancelled: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        if cancelled.load(Ordering::SeqCst) {
            if let Some(tx) = popup.tx.take() {
                let _ = tx.send(Answer::Cancelled);
            }
            return Ok(());
        }
        let inst = GetModuleHandleW(None)?;
        let class = w!("remote-control-consent");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: theme::icon(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let ptr = Box::into_raw(Box::new(popup));
        let style = WS_CAPTION | WS_SYSMENU;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            &HSTRING::from(tr("远程控制", "Remote Control")),
            style,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(inst.into()),
            Some(ptr as *const _),
        );
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                if let Some(tx) = Box::from_raw(ptr).tx.take() {
                    let _ = tx.send(Answer::NoAnswer);
                }
                return Err(e);
            }
        };
        theme::frame(hwnd, theme::CHROME);
        let px = gdi::scale(hwnd);
        let mut r = RECT { left: 0, top: 0, right: px(WIDTH), bottom: px(HEIGHT) };
        let dpi = GetDpiForWindow(hwnd).max(96);
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, WS_EX_TOPMOST, dpi);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        if cancelled.load(Ordering::SeqCst) {
            finish(hwnd, Answer::Cancelled);
            return Ok(());
        }
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), (sw - w) / 2, (sh - h) / 2, w, h, SWP_SHOWWINDOW);
        if let Some(p) = self::popup(hwnd) {
            p.shown_at = Instant::now();
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            let _ = FlashWindow(hwnd, true);
        }
        SetTimer(Some(hwnd), TIMER, 1000, None);
        hwnd_out.store(hwnd.0 as isize, Ordering::SeqCst);
        if cancelled.load(Ordering::SeqCst) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(1), LPARAM(0));
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        hwnd_out.store(0, Ordering::SeqCst);
        Ok(())
    }
}

/// The window's popup; valid until WM_NCDESTROY.
unsafe fn popup<'a>(hwnd: HWND) -> Option<&'a mut Popup> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Popup).as_mut() }
}

/// Sends the answer and closes; the popup is gone afterwards.
unsafe fn finish(hwnd: HWND, answer: Answer) {
    unsafe {
        if let Some(p) = popup(hwnd)
            && let Some(tx) = p.tx.take()
        {
            let _ = tx.send(answer);
        }
        let _ = DestroyWindow(hwnd);
    }
}

fn xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = &*(lp.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_PAINT => {
                if let Some(p) = popup(hwnd) {
                    paint(hwnd, p);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEMOVE => {
                if let Some(p) = popup(hwnd) {
                    let (x, y) = xy(lp);
                    let h = p.hit(hwnd, x, y);
                    if h != p.hover {
                        p.hover = h;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut t);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                if let Some(p) = popup(hwnd) {
                    let changed = p.hover.take().is_some() || p.pressed.take().is_some();
                    if changed {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                if let Some(p) = popup(hwnd)
                    && p.pressed.take().is_some()
                {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
                let hand = popup(hwnd).is_some_and(|p| p.hover.is_some());
                let _ = SetCursor(LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).ok());
                LRESULT(1)
            }
            WM_LBUTTONDOWN => {
                if let Some(p) = popup(hwnd) {
                    let (x, y) = xy(lp);
                    let h = p.hit(hwnd, x, y);
                    if h == Some(Hit::Allow) && p.shown_at.elapsed() < COOLDOWN {
                        return LRESULT(0);
                    }
                    if h != p.pressed {
                        p.pressed = h;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    if h.is_some() {
                        SetCapture(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = xy(lp);
                let Some(p) = popup(hwnd) else { return LRESULT(0) };
                let was = p.pressed.take();
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                let hit_now = p.hit(hwnd, x, y);
                if hit_now == Some(Hit::Allow) && p.shown_at.elapsed() < COOLDOWN {
                    if was.is_some() {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
                if let (Some(w), Some(now)) = (was, hit_now)
                    && w == now
                {
                    match now {
                        Hit::Allow => finish(hwnd, Answer::Allow),
                        Hit::Deny => finish(hwnd, Answer::Deny),
                    }
                } else if was.is_some() {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
                finish(hwnd, Answer::Deny);
                LRESULT(0)
            }
            WM_TIMER => {
                if let Some(p) = popup(hwnd) {
                    p.left_s = p.left_s.saturating_sub(1);
                    if p.left_s == 0 {
                        finish(hwnd, Answer::NoAnswer);
                        return LRESULT(0);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                finish(hwnd, if wp.0 == 1 { Answer::Cancelled } else { Answer::Deny });
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER);
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Popup;
                if !p.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let mut b = Box::from_raw(p);
                    if let Some(tx) = b.tx.take() {
                        let _ = tx.send(Answer::Cancelled);
                    }
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn paint(hwnd: HWND, p: &Popup) {
    let px = gdi::scale(hwnd);
    let r = |l: i32, t: i32, rr: i32, b: i32| rect(px(l), px(t), px(rr), px(b));
    let mut c = Canvas::begin(hwnd, gdi::BG);
    let (title, body, bold, small) = (c.font(px(17), true), c.font(px(14), false), c.font(px(14), true), c.font(px(12), false));
    let heading = if is_en() {
        format!("{} wants to control this PC", p.name)
    } else {
        format!("「{}」想控制这台电脑", p.name)
    };
    c.text(&r(24, 20, WIDTH - 24, 48), &heading, title, gdi::TEXT, false);
    let what = match p.kind {
        Kind::Trusted => tr("信任设备", "Trusted device"),
        Kind::New { via_code: true } => tr("临时设备 · 经连接码", "Temporary device · via code"),
        Kind::New { via_code: false } => tr("临时设备 · 来自局域网", "Temporary device · on LAN"),
    };
    c.text(&r(24, 50, WIDTH - 24, 72), what, body, gdi::DIM, false);
    // How long until it counts as refused.
    let (bl, br) = (24, WIDTH - 24);
    c.round(&r(bl, 86, br, 89), px(2), Some(gdi::BUTTON_EDGE), None);
    let left = bl + (br - bl) * p.left_s as i32 / p.wait_s.max(1) as i32;
    if left > bl {
        c.round(&r(bl, 86, left, 89), px(2), Some(AMBER), None);
    }
    for (hit, a) in p.layout() {
        let hovered = p.hover == Some(hit);
        let b = r(a.left, a.top, a.right, a.bottom);
        match hit {
            Hit::Allow => {
                c.round(&b, px(6), Some(if hovered { gdi::ACCENT_HOVER } else { gdi::ACCENT }), None);
                c.text(&b, tr("允许", "Allow"), bold, gdi::TEXT, true);
            }
            Hit::Deny => {
                c.round(&b, px(6), Some(if hovered { gdi::DANGER_BTN_HOVER } else { gdi::DANGER_BTN }), None);
                c.text(&b, tr("拒绝", "Deny"), bold, gdi::TEXT, true);
            }
        }
    }
    let by = HEIGHT - 20 - 36;
    let countdown = if is_en() {
        format!("Auto-deny in {}s", p.left_s)
    } else {
        format!("{} 秒没人点就算拒绝", p.left_s)
    };
    c.text(&r(24, by, 206, by + 36), &countdown, small, gdi::DIM, false);
}
