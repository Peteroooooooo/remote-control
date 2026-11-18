//! The popup on the host asking whether a device may control this computer
//! (`target-plan` 2). It runs on its own thread with its own message loop,
//! so the lobby and the service's helper can both show it; in the helper the
//! thread first joins the input desktop, where the user is. Enter does
//! nothing (a key typed by chance must not let anyone in); Esc and the close
//! button refuse; no answer within the countdown refuses too.

use std::{
    sync::{
        Arc,
        atomic::{AtomicIsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::oneshot;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::InvalidateRect,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE},
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, w},
};

use crate::{
    gdi::{self, Canvas, rect},
    log, theme,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Not in the device list: met with a connection code, or on the LAN.
    New { via_code: bool },
    Trusted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Deny,
    /// Allowed; a new device stays temporary.
    Once,
    /// Allowed; trusted from now on (asks each time).
    Trust,
    /// Allowed; "my computer" from now on (never asks again).
    Mine,
    NoAnswer,
    /// Closed by the program (the viewer gave up).
    Cancelled,
}

pub struct Asking {
    hwnd: Arc<AtomicIsize>,
    pub rx: oneshot::Receiver<Answer>,
}

impl Asking {
    pub fn cancel(&self) {
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
    Choice(Answer),
    Check,
    Allow,
    Deny,
}

struct Popup {
    name: String,
    kind: Kind,
    /// For a new device: Once, Trust or Mine.
    choice: Answer,
    /// For a trusted device: "from now on without asking".
    checked: bool,
    hover: Option<Hit>,
    left_s: u32,
    tx: Option<oneshot::Sender<Answer>>,
}

/// Width of the client area in window units.
const WIDTH: i32 = 500;

impl Popup {
    fn height(&self) -> i32 {
        match self.kind {
            Kind::New { .. } => 196,
            Kind::Trusted => 164,
        }
    }

    /// Where everything goes, in window units.
    fn layout(&self) -> Vec<(Hit, RECT)> {
        let by = self.height() - 24 - 36;
        let mut v = Vec::new();
        if let Kind::New { .. } = self.kind {
            for (i, a) in [Answer::Once, Answer::Trust, Answer::Mine].into_iter().enumerate() {
                let x = 74 + 112 * i as i32;
                v.push((Hit::Choice(a), rect(x, 86, x + 104, 114)));
            }
        } else {
            v.push((Hit::Check, rect(24, 60, WIDTH - 24, 88)));
        }
        v.push((Hit::Allow, rect(WIDTH - 24 - 96 - 12 - 124, by, WIDTH - 24 - 96 - 12, by + 36)));
        v.push((Hit::Deny, rect(WIDTH - 24 - 96, by, WIDTH - 24, by + 36)));
        v
    }

    fn hit(&self, hwnd: HWND, x: i32, y: i32) -> Option<Hit> {
        let px = gdi::scale(hwnd);
        self.layout().into_iter().find(|(_, r)| gdi::inside(&rect(px(r.left), px(r.top), px(r.right), px(r.bottom)), x, y)).map(|(h, _)| h)
    }
}

/// Shows the popup; the answer arrives on `Asking::rx`.
pub fn ask(name: &str, kind: Kind, wait: Duration) -> Asking {
    let hwnd = Arc::new(AtomicIsize::new(0));
    let (tx, rx) = oneshot::channel();
    let popup = Popup {
        name: name.to_string(),
        kind,
        choice: Answer::Once,
        checked: false,
        hover: None,
        left_s: wait.as_secs().max(1) as u32,
        tx: Some(tx),
    };
    let h = hwnd.clone();
    let r = std::thread::Builder::new().name("consent".into()).spawn(move || {
        if let Err(e) = crate::desktop::follow() {
            log!("confirmation popup: {e:#}");
        }
        if let Err(e) = run(popup, &h) {
            log!("confirmation popup could not open: {e:#}");
        }
    });
    if let Err(e) = r {
        log!("confirmation popup thread: {e}");
    }
    Asking { hwnd, rx }
}

const TIMER: usize = 1;

fn run(popup: Popup, hwnd_out: &AtomicIsize) -> windows::core::Result<()> {
    unsafe {
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
        let height = popup.height();
        let ptr = Box::into_raw(Box::new(popup));
        let style = WS_CAPTION | WS_SYSMENU;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            &HSTRING::from("remote control"),
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
        let mut r = RECT { left: 0, top: 0, right: px(WIDTH), bottom: px(height) };
        let _ = AdjustWindowRectEx(&mut r, style, false, WS_EX_TOPMOST);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), (sw - w) / 2, (sh - h) / 2, w, h, SWP_SHOWWINDOW);
        if !SetForegroundWindow(hwnd).as_bool() {
            let _ = FlashWindow(hwnd, true);
        }
        SetTimer(Some(hwnd), TIMER, 1000, None);
        hwnd_out.store(hwnd.0 as isize, Ordering::SeqCst);
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
            0x02A3 /* WM_MOUSELEAVE */ => {
                if let Some(p) = popup(hwnd)
                    && p.hover.take().is_some()
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
            WM_LBUTTONUP => {
                let (x, y) = xy(lp);
                let Some(p) = popup(hwnd) else { return LRESULT(0) };
                match p.hit(hwnd, x, y) {
                    Some(Hit::Choice(a)) => p.choice = a,
                    Some(Hit::Check) => p.checked = !p.checked,
                    Some(Hit::Allow) => {
                        let a = match p.kind {
                            Kind::New { .. } => p.choice,
                            Kind::Trusted if p.checked => Answer::Mine,
                            Kind::Trusted => Answer::Trust,
                        };
                        finish(hwnd, a);
                        return LRESULT(0);
                    }
                    Some(Hit::Deny) => {
                        finish(hwnd, Answer::Deny);
                        return LRESULT(0);
                    }
                    None => {}
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
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
    let head = match p.kind {
        Kind::Trusted => format!("「{}」请求控制你的电脑（信任设备）", p.name),
        Kind::New { .. } => format!("「{}」请求控制你的电脑", p.name),
    };
    c.text(&r(24, 18, WIDTH - 24, 46), &head, title, gdi::TEXT, false);
    if let Kind::New { via_code } = p.kind {
        let from = if via_code { "新设备（通过连接码）" } else { "新设备（来自局域网）" };
        c.text(&r(24, 46, WIDTH - 24, 68), from, body, gdi::DIM, false);
        c.text(&r(24, 86, 74, 114), "以后：", body, gdi::TEXT, false);
    }
    for (hit, a) in p.layout() {
        let hovered = p.hover == Some(hit);
        let cy = (px(a.top) + px(a.bottom)) / 2;
        match hit {
            Hit::Choice(choice) => {
                let on = p.choice == choice;
                let ring = if on { gdi::ACCENT } else if hovered { gdi::TEXT } else { gdi::DIM };
                c.circle(px(a.left + 8), cy, px(7), None, Some((ring, px(2).max(1))));
                if on {
                    c.circle(px(a.left + 8), cy, px(4), Some(gdi::ACCENT), None);
                }
                let label = match choice {
                    Answer::Trust => "信任设备",
                    Answer::Mine => "我的电脑",
                    _ => "只这一次",
                };
                c.text(&r(a.left + 22, a.top, a.right, a.bottom), label, body, gdi::TEXT, false);
            }
            Hit::Check => {
                let b = rect(px(a.left), cy - px(8), px(a.left + 16), cy + px(8));
                let (fill, ring) = if p.checked { (Some(gdi::ACCENT), gdi::ACCENT) } else { (None, if hovered { gdi::TEXT } else { gdi::DIM }) };
                c.round(&b, px(4), fill, Some((ring, px(2).max(1))));
                if p.checked {
                    c.text(&b, "✓", bold, gdi::TEXT, true);
                }
                c.text(&r(a.left + 26, a.top, a.right, a.bottom), "以后它控制我时免确认（改为我的电脑）", body, gdi::TEXT, false);
            }
            Hit::Allow => {
                c.round(&r(a.left, a.top, a.right, a.bottom), px(6), Some(if hovered { gdi::ACCENT_HOVER } else { gdi::ACCENT }), None);
                c.text(&r(a.left, a.top, a.right, a.bottom), "允许控制", bold, gdi::TEXT, true);
            }
            Hit::Deny => {
                let fill = if hovered { gdi::BUTTON_HOVER } else { gdi::BUTTON };
                c.round(&r(a.left, a.top, a.right, a.bottom), px(6), Some(fill), Some((gdi::BUTTON_EDGE, 1)));
                c.text(&r(a.left, a.top, a.right, a.bottom), "拒绝", bold, gdi::TEXT, true);
            }
        }
    }
    let by = p.height() - 24 - 36;
    c.text(&r(24, by, 220, by + 36), &format!("{} 秒后自动拒绝", p.left_s), small, gdi::DIM, false);
}
