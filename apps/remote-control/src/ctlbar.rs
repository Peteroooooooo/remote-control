//! The bar at the top of the host's screen while it is being controlled
//! (`target-plan` 6.5): "「name」正在控制 00:12 [断开]". It shows in full for
//! a moment when the session starts, then folds up into a thin red tab at
//! the top edge, so it covers nothing; it slides down again while the
//! pointer is at the top edge above it or on it, and folds up a second
//! after the pointer leaves. Only its 断开 button ends the session.
//!
//! It answers only the mouse of the person at this computer: a low-level
//! mouse hook tells their moves from the viewer's (injected ones). While the
//! viewer moves the pointer the bar stays folded and lets clicks through to
//! what is under it, so the viewer neither opens it nor clicks it by
//! mistake (the viewer has its own controls).
//!
//! A layered window (see-through, and click-through, outside its shape) with
//! its own thread and message loop, like the confirmation popup.

use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, w},
};

use crate::{
    gdi::{self, Canvas, coverage, rect, rgb},
    i18n::{is_en, tr},
    log, theme,
    window::HOOK_TEST_MARK,
};

pub struct Bar {
    hwnd: Arc<AtomicIsize>,
    gone: Arc<AtomicBool>,
}

impl Drop for Bar {
    fn drop(&mut self) {
        self.gone.store(true, Ordering::SeqCst);
        let h = self.hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Window units (96 DPI).
const HEIGHT: i32 = 40;
const RADIUS: i32 = 12;
const MIN_BUTTON_W: i32 = 60;
const BUTTON_H: i32 = 26;
const TAB_W: i32 = 64;
const TAB_H: i32 = 6;
/// Shown in full this long when the session starts.
const FIRST_SHOW: Duration = Duration::from_millis(1500);
/// Folds up this long after the pointer left.
const LINGER: Duration = Duration::from_secs(1);
const SLIDE: f32 = 0.26;

const TIMER_ANIMATE: usize = 1;
const TIMER_WATCH: usize = 2;

/// The latest mouse event came from the person at this computer (not the viewer).
static LOCAL: AtomicBool = AtomicBool::new(true);

thread_local! {
    /// The bar and the mouse hook, on the bar's thread.
    static BAR: Cell<HWND> = Cell::new(HWND::default());
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

const FILL: u32 = 0x1b1c20;
const EDGE: u32 = 0x3a3c42;
const RED: u32 = 0xe5534b;
const DANGER: u32 = 0xff8a83;

struct State {
    name: String,
    on_click: Box<dyn Fn() + Send>,
    started: Instant,
    /// Width in pixels, and the parts' places (from `layout`).
    width: i32,
    text_w: i32,
    clock_w: i32,
    button_w: i32,
    /// 0 folded up, 1 down.
    t: f32,
    open: bool,
    last_frame: Instant,
    /// The pointer was at the top edge or on the bar.
    last_near: Instant,
    hover_button: bool,
    pressed: bool,
    /// The timer's text last drawn.
    shown_clock: String,
    x: i32,
    y: i32,
}

/// Shows the bar until the returned value is dropped.
pub fn show(name: &str, on_click: impl Fn() + Send + 'static) -> Bar {
    let hwnd = Arc::new(AtomicIsize::new(0));
    let now = Instant::now();
    let st = State {
        name: name.to_string(),
        on_click: Box::new(on_click),
        started: now,
        width: 0,
        text_w: 0,
        clock_w: 0,
        button_w: 0,
        t: 0.0,
        open: true,
        last_frame: now,
        last_near: now,
        hover_button: false,
        pressed: false,
        shown_clock: String::new(),
        x: 0,
        y: 0,
    };
    let gone = Arc::new(AtomicBool::new(false));
    let (h, g) = (hwnd.clone(), gone.clone());
    let r = std::thread::Builder::new().name("control-bar".into()).spawn(move || {
        if let Err(e) = crate::desktop::follow() {
            log!("control bar: {e:#}");
        }
        if let Err(e) = run(st, &h, &g) {
            log!("control bar could not open: {e:#}");
        }
    });
    if let Err(e) = r {
        log!("control bar thread: {e}");
    }
    Bar { hwnd, gone }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

fn label(name: &str) -> String {
    if is_en() {
        format!("{name} is controlling this PC")
    } else {
        format!("「{name}」正在控制")
    }
}

fn button_label() -> &'static str {
    tr("断开", "Disconnect")
}

fn recompute_layout(hwnd: HWND, st: &mut State) {
    let px = gdi::scale(hwnd);
    let mut m = Canvas::offscreen(1, 1, rgb(FILL));
    let f = m.font(px(14), false);
    st.text_w = m.text_width(&label(&st.name), f);
    st.clock_w = m.text_width("00:00:00", f);
    st.button_w = px(MIN_BUTTON_W).max(m.text_width(button_label(), f) + px(20));
    st.width = px(30) + st.text_w + px(12) + st.clock_w + px(12) + st.button_w + px(8);
    st.x = (unsafe { GetSystemMetrics(SM_CXSCREEN) } - st.width) / 2;
    st.y = 0;
}

fn run(mut st: State, hwnd_out: &AtomicIsize, gone: &AtomicBool) -> windows::core::Result<()> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("remote-control-bar");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let hwnd = CreateWindowExW(ex, class, &HSTRING::from("remote control"), WS_POPUP, 0, 0, 10, 10, None, None, Some(inst.into()), None)?;
        recompute_layout(hwnd, &mut st);
        let ptr = Box::into_raw(Box::new(st));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        draw(hwnd, &mut *ptr);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetTimer(Some(hwnd), TIMER_ANIMATE, 15, None);
        SetTimer(Some(hwnd), TIMER_WATCH, 50, None);
        hwnd_out.store(hwnd.0 as isize, Ordering::SeqCst);
        BAR.set(hwnd);
        LOCAL.store(true, Ordering::Relaxed);
        match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(inst.into()), 0) {
            Ok(h) => HOOK.set(Some(h)),
            Err(e) => log!("control bar: no mouse hook, it answers every pointer: {e}"),
        }
        if gone.load(Ordering::SeqCst) {
            let _ = DestroyWindow(hwnd);
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Some(h) = HOOK.take() {
            let _ = UnhookWindowsHookEx(h);
        }
        hwnd_out.store(0, Ordering::SeqCst);
        Ok(())
    }
}

/// How far the bar is down, eased.
fn eased(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// The button's place in the window, with the bar `off` pixels up.
fn button_rect(st: &State, px: &impl Fn(i32) -> i32, off: i32) -> windows::Win32::Foundation::RECT {
    let (bw, bh) = (st.button_w, px(BUTTON_H));
    let l = st.width - px(8) - bw;
    let t = (px(HEIGHT) - bh) / 2 - off;
    rect(l, t, l + bw, t + bh)
}

fn offset(st: &State, px: &impl Fn(i32) -> i32) -> i32 {
    ((1.0 - eased(st.t)) * (px(HEIGHT) + 2) as f32).round() as i32
}

fn draw(hwnd: HWND, st: &mut State) {
    let px = gdi::scale(hwnd);
    let (w, h) = (st.width, px(HEIGHT));
    let off = offset(st, &px);
    let mut c = Canvas::offscreen(w, h + 1, rgb(FILL));
    let tab = [((w - px(TAB_W)) / 2) as f32, -(px(TAB_H) as f32), ((w + px(TAB_W)) / 2) as f32, px(TAB_H) as f32];
    c.round(&rect(tab[0] as i32, 0, tab[2] as i32, px(TAB_H)), px(3), Some(rgb(RED)), None);
    let r = px(RADIUS);
    let pill = [0.0, (-off - r) as f32, w as f32, (h - off) as f32];
    c.round(&rect(0, -off - r, w, h - off), r, Some(rgb(FILL)), Some((rgb(EDGE), 1)));
    let cy = h / 2 - off;
    c.circle(px(16), cy, px(4), Some(rgb(RED)), None);
    let f = c.font(px(14), false);
    c.text(&rect(px(30), cy - h / 2, px(30) + st.text_w + 2, cy + h / 2), &label(&st.name), f, gdi::TEXT, false);
    st.shown_clock = clock(st.started.elapsed());
    let cx = px(30) + st.text_w + px(12);
    c.text(&rect(cx, cy - h / 2, cx + st.clock_w + 2, cy + h / 2), &st.shown_clock, f, gdi::DIM, false);
    let b = button_rect(st, &px, off);
    let fill = if st.hover_button {
        if st.pressed { rgb(0x4a2b2c) } else { rgb(0x5a2f30) }
    } else {
        rgb(theme::BUTTON_HOVER)
    };
    c.round(&b, px(6), Some(fill), None);
    c.text(&b, button_label(), f, rgb(DANGER), true);
    let tab_a = 1.0 - eased(st.t);
    let (rr, tr) = (r as f32, px(3) as f32);
    c.present_layered(hwnd, st.x, st.y, |x, y| {
        let a = coverage(pill, rr, x, y);
        let t = coverage(tab, tr, x, y) * tab_a;
        a + t * (1.0 - a)
    });
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut State> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State).as_mut() }
}

/// Sees every mouse event before it moves the pointer. The viewer's come
/// injected: from them on the bar lets clicks through, until the person
/// here moves the mouse again. (Changing the style here, before the event
/// goes on, means the click that follows already passes through.)
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let m = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        let injected = m.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
        let local = !injected || m.dwExtraInfo == HOOK_TEST_MARK;
        if LOCAL.swap(local, Ordering::Relaxed) != local {
            let hwnd = BAR.get();
            unsafe {
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let t = WS_EX_TRANSPARENT.0 as isize;
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, if local { ex & !t } else { ex | t });
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// The local person's pointer is at the top edge above the bar, or on it.
fn near(hwnd: HWND, st: &State) -> bool {
    if !LOCAL.load(Ordering::Relaxed) {
        return false;
    }
    let px = gdi::scale(hwnd);
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return false;
    }
    let (l, r) = (st.x - px(40), st.x + st.width + px(40));
    let at_edge = p.y >= st.y && p.y <= st.y + px(TAB_H) && p.x >= l && p.x < r;
    let down = st.t > 0.5 && p.x >= st.x && p.x < st.x + st.width && p.y >= st.y && p.y < st.y + px(HEIGHT) + px(6);
    at_edge || down
}

fn watch(hwnd: HWND, st: &mut State) {
    let now = Instant::now();
    if near(hwnd, st) {
        st.last_near = now;
    }
    let open = st.started.elapsed() < FIRST_SHOW || now - st.last_near < LINGER;
    if open != st.open {
        st.open = open;
        st.last_frame = now;
        unsafe {
            SetTimer(Some(hwnd), TIMER_ANIMATE, 15, None);
        }
    }
    if st.t > 0.0 && clock(st.started.elapsed()) != st.shown_clock {
        draw(hwnd, st);
    }
}

fn animate(hwnd: HWND, st: &mut State) {
    let now = Instant::now();
    let dt = (now - st.last_frame).as_secs_f32().min(0.05);
    st.last_frame = now;
    let target = if st.open { 1.0 } else { 0.0 };
    let step = dt / SLIDE;
    st.t = if st.t < target { (st.t + step).min(target) } else { (st.t - step).max(target) };
    draw(hwnd, st);
    if st.t == target {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_ANIMATE);
        }
    }
}

fn on_button(hwnd: HWND, st: &State, lp: LPARAM) -> bool {
    let px = gdi::scale(hwnd);
    let (x, y) = ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    st.t > 0.9 && gdi::inside(&button_rect(st, &px, offset(st, &px)), x, y)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                if let Some(s) = state(hwnd) {
                    match wp.0 {
                        TIMER_ANIMATE => animate(hwnd, s),
                        TIMER_WATCH => watch(hwnd, s),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_SETCURSOR => {
                let hand = state(hwnd).is_some_and(|s| s.hover_button);
                if let Ok(c) = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }) {
                    SetCursor(Some(c));
                }
                LRESULT(1)
            }
            WM_MOUSEMOVE => {
                if let Some(s) = state(hwnd) {
                    let on = on_button(hwnd, s, lp);
                    if on != s.hover_button {
                        s.hover_button = on;
                        draw(hwnd, s);
                    }
                    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut t);
                }
                LRESULT(0)
            }
            0x02A3 /* WM_MOUSELEAVE */ => {
                if let Some(s) = state(hwnd)
                    && (s.hover_button || s.pressed)
                {
                    s.hover_button = false;
                    s.pressed = false;
                    draw(hwnd, s);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                if let Some(s) = state(hwnd)
                    && on_button(hwnd, s, lp)
                {
                    s.pressed = true;
                    draw(hwnd, s);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if let Some(s) = state(hwnd) {
                    let was = std::mem::take(&mut s.pressed);
                    if was && on_button(hwnd, s, lp) {
                        (s.on_click)();
                    }
                    draw(hwnd, s);
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                if let Some(s) = state(hwnd) {
                    recompute_layout(hwnd, s);
                    draw(hwnd, s);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
                if !p.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(p));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
