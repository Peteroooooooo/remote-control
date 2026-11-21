//! The viewer's controls: the line's quality ("直连 · 6 ms", a click shows
//! the numbers), switch display, Ctrl+Alt+Del, release keyboard and mouse,
//! 画面 (resolution, frame rate and bitrate, see quality.rs), terminal,
//! full screen, 断开. In a window they sit in the middle of its title bar, which is drawn
//! here along with the minimise, maximise and close buttons (window.rs hands
//! over the title bar's painting, hit testing and mouse). In full screen they
//! move into a bar at the top edge that shows for a moment, then slides up
//! leaving a 4 px edge; it comes down while the pointer is at the top edge
//! above it or on it, and goes up a moment after the pointer leaves. It stays
//! down while the numbers or the 画面 menu are open, input is released, the
//! picture is paused and once the session has ended.
//!
//! The numbers, the 画面 menu, the tips and the full-screen bar are in a layered window
//! owned by the session window (above it, gone with it), see-through and
//! click-through outside its shapes. It lives on the session window's thread
//! and message loop.

use std::{
    cell::RefCell,
    sync::{
        Mutex,
        atomic::{AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            ClientToScreen, GetMonitorInfoW, HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromWindow, ScreenToClient,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                GetCapture, ReleaseCapture, SetCapture, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, w},
};

use crate::{
    audio,
    gdi::{self, Canvas, coverage, rect, rgb},
    keys, log,
    proto::AudioCfg,
    quality::{self, Res},
    theme, window,
};

/// The title bar's height (window units, 96 DPI, as below).
pub const CAPTION: i32 = 32;
/// A caption button (minimise, maximise, close).
const CAP_W: i32 = 46;
const BAR_H: i32 = 40;
const RADIUS: i32 = 12;
/// What is left of the full-screen bar when it is up.
const HANDLE: i32 = 4;
const STATS_W: i32 = 340;
const ROW_H: i32 = 22;
/// The 画面 menu: a row of chips per setting, its name on the left.
const MENU_ROW: i32 = 34;
const MENU_LABEL_W: i32 = 52;
const CHIP_H: i32 = 26;
/// The full-screen bar shows this long when full screen starts.
const FIRST_SHOW: Duration = Duration::from_millis(1500);
/// Goes up this long after the pointer left.
const LINGER: Duration = Duration::from_millis(1200);
const TIP_DELAY: Duration = Duration::from_millis(500);
const SLIDE: f32 = 0.26;
const RELEASED: &str = "键鼠已释放，点画面继续";

const TIMER_ANIMATE: usize = 1;
const TIMER_WATCH: usize = 2;
const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_FOLLOW: u32 = WM_APP + 2;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_NCMOUSELEAVE: u32 = 0x02A2;

const FILL: u32 = theme::CHROME;
const EDGE: u32 = 0x3a3c42;
const GREEN: u32 = 0x3fb950;
const AMBER: u32 = 0xd29922;
const RED: u32 = 0xe5534b;
const DANGER: u32 = 0xff8a83;
const CLOSE_HOT: u32 = 0xc42b1c;
const CLOSE_DOWN: u32 = 0xa82a1c;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Fair,
    Bad,
}

struct Info {
    tone: Tone,
    badge: String,
    rows: Vec<(String, String)>,
    /// Keep the full-screen bar down (picture paused, session over).
    hold: bool,
}

static INFO: Mutex<Info> = Mutex::new(Info { tone: Tone::Fair, badge: String::new(), rows: Vec::new(), hold: false });
static TOOLBAR: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Badge,
    Monitor,
    Cad,
    Quality,
    Audio,
    AudioMenu,
    Terminal,
    Full,
    Disconnect,
}

/// A choice in the 画面 or 声音 menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chip {
    Res(Res),
    Fps(u8),
    Mbps(u16),
    Sharp(bool),
    AudioLossless(bool),
    AudioCh(u8),
    AudioMuteHost(bool),
}

impl Chip {
    fn on(self, q: &quality::Choice, a: &AudioCfg) -> bool {
        match self {
            Chip::Res(r) => q.res == r,
            Chip::Fps(f) => q.fps == f,
            Chip::Mbps(m) => q.mbps == m,
            Chip::Sharp(s) => q.sharp == s,
            Chip::AudioLossless(l) => a.lossless == l,
            Chip::AudioCh(ch) => a.channels == ch,
            Chip::AudioMuteHost(m) => a.mute_host == m,
        }
    }

    fn choose(self) {
        match self {
            Chip::Res(r) => quality::choose(|c| c.res = r),
            Chip::Fps(f) => quality::choose(|c| c.fps = f),
            Chip::Mbps(m) => quality::choose(|c| c.mbps = m),
            Chip::Sharp(s) => quality::choose(|c| c.sharp = s),
            Chip::AudioLossless(l) => audio::config::choose(|c| c.lossless = l),
            Chip::AudioCh(ch) => audio::config::choose(|c| c.channels = ch),
            Chip::AudioMuteHost(m) => audio::config::choose(|c| c.mute_host = m),
        }
    }
}

/// Something under the pointer that reacts to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hot {
    Part(Part),
    Min,
    Max,
    Close,
}

/// Sizes of the controls (window units): smaller in the title bar.
struct Metrics {
    font: i32,
    badge_h: i32,
    icon_w: i32,
    icon_h: i32,
    end_w: i32,
    end_h: i32,
    sep_h: i32,
}

const IN_CAPTION: Metrics = Metrics { font: 12, badge_h: 24, icon_w: 30, icon_h: 26, end_w: 48, end_h: 22, sep_h: 14 };
const IN_BAR: Metrics = Metrics { font: 13, badge_h: 28, icon_w: 30, icon_h: 30, end_w: 56, end_h: 26, sep_h: 20 };

/// What the controls show right now.
struct Look {
    tone: Tone,
    badge: String,
    rows: Vec<(String, String)>,
    hold: bool,
    input: bool,
    released: bool,
    full: bool,
    monitors: usize,
    audio_enabled: bool,
    audio_mute_host: bool,
}

fn look() -> Look {
    let i = INFO.lock().unwrap();
    let badge = if i.badge.is_empty() { "正在连接…".to_string() } else { i.badge.clone() };
    let input = window::input();
    Look {
        tone: i.tone,
        badge,
        rows: i.rows.clone(),
        hold: i.hold,
        input,
        released: input && !window::captured(),
        full: window::fullscreen(),
        monitors: window::monitors().1,
        audio_enabled: audio::config::viewer_enabled(),
        audio_mute_host: audio::config::host_muted(),
    }
}

#[derive(Clone, Copy)]
struct SliderRects {
    hit: RECT,
    track: RECT,
}

struct State {
    /// The session window.
    owner: HWND,
    /// Laid out for full screen (the bar) rather than the title bar.
    full: bool,
    /// When full screen started.
    started: Instant,
    /// The full-screen bar: 0 up, 1 down.
    t: f32,
    open: bool,
    last_frame: Instant,
    last_near: Instant,
    stats: bool,
    /// The 画面 menu is open; its chips in this window's pixels.
    menu: bool,
    /// The 声音 menu is open.
    audio_menu: bool,
    chips: Vec<(Chip, RECT)>,
    chip_hover: Option<Chip>,
    chip_pressed: Option<Chip>,
    /// The 声音 menu's volume slider in this window's pixels.
    slider: Option<SliderRects>,
    slider_hover: bool,
    slider_drag: bool,
    hover: Option<(Hot, Instant)>,
    pressed: Option<Hot>,
    r_pressed: Option<Hot>,
    tip_shown: bool,
    /// The title bar's controls, in the session window's client pixels.
    caption: Vec<(Hot, RECT)>,
    /// What the last drawing of this window put where, in its pixels, and
    /// where it is on the screen.
    parts: Vec<(Part, RECT)>,
    bar: RECT,
    panel: Option<RECT>,
    origin: POINT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The latest numbers (any thread).
pub fn update(tone: Tone, badge: String, rows: Vec<(String, String)>, hold: bool) {
    *INFO.lock().unwrap() = Info { tone, badge, rows, hold };
    post(WM_APP_UPDATE);
}

/// The session ended: `why` in red, until the window closes (any thread).
pub fn closed(why: &str) {
    {
        let mut i = INFO.lock().unwrap();
        i.tone = Tone::Bad;
        i.badge = why.to_string();
        i.hold = true;
    }
    post(WM_APP_UPDATE);
}

/// Draw again: capture, displays, title or full screen changed (any thread).
pub fn refresh() {
    post(WM_APP_UPDATE);
}

/// The session window moved or changed size.
pub fn follow() {
    post(WM_APP_FOLLOW);
}

/// The picture was clicked: the numbers and the menu close (session window's thread).
pub fn dismiss() {
    if with(|s| {
        let closed = std::mem::take(&mut s.stats) | std::mem::take(&mut s.menu) | std::mem::take(&mut s.audio_menu);
        if closed {
            s.slider = None;
            s.slider_hover = false;
            s.slider_drag = false;
        }
        closed
    })
    .unwrap_or(false)
    {
        changed();
    }
}

fn post(msg: u32) {
    let h = toolbar();
    if !h.is_invalid() {
        unsafe {
            let _ = PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn toolbar() -> HWND {
    HWND(TOOLBAR.load(Ordering::Relaxed) as *mut _)
}

/// Opens the layered window over `owner`, on its thread.
pub fn create(owner: HWND) {
    if let Err(e) = open(owner) {
        log!("toolbar could not open: {e}");
    }
}

fn open(owner: HWND) -> windows::core::Result<()> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("remote-control-toolbar");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let hwnd =
            CreateWindowExW(ex, class, &HSTRING::from("remote control toolbar"), WS_POPUP, 0, 0, 10, 10, Some(owner), None, Some(inst.into()), None)?;
        let now = Instant::now();
        STATE.set(Some(State {
            owner,
            full: false,
            started: now,
            t: 0.0,
            open: false,
            last_frame: now,
            last_near: now,
            stats: false,
            menu: false,
            audio_menu: false,
            chips: Vec::new(),
            chip_hover: None,
            chip_pressed: None,
            slider: None,
            slider_hover: false,
            slider_drag: false,
            hover: None,
            pressed: None,
            r_pressed: None,
            tip_shown: false,
            caption: Vec::new(),
            parts: Vec::new(),
            bar: RECT::default(),
            panel: None,
            origin: POINT::default(),
        }));
        TOOLBAR.store(hwnd.0 as isize, Ordering::Relaxed);
        SetTimer(Some(hwnd), TIMER_WATCH, 50, None);
        Ok(())
    }
}

fn eased(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
}

fn redraw(hwnd: HWND) {
    with(|s| draw(hwnd, s));
}

/// Something shown changed: draw this window and the title bar again.
fn changed() {
    redraw(toolbar());
    if let Some(owner) = with(|s| s.owner) {
        invalidate_caption(owner);
    }
}

fn invalidate_caption(owner: HWND) {
    let px = gdi::scale(owner);
    let mut c = RECT::default();
    unsafe {
        let _ = GetClientRect(owner, &mut c);
        c.bottom = c.bottom.min(px(CAPTION));
        let _ = InvalidateRect(Some(owner), Some(&c), false);
    }
}

fn tip(part: Part, look: &Look) -> Option<String> {
    Some(match part {
        Part::Badge | Part::Disconnect => return None,
        Part::Monitor => {
            let (current, count) = window::monitors();
            format!("显示器 {}/{count}（Ctrl+Alt+Shift+→）", current + 1)
        }
        Part::Cad => "发送 Ctrl+Alt+Del（Ctrl+Alt+End）".into(),
        Part::Quality => "画面：分辨率、帧率、码率".into(),
        Part::Audio => {
            let main = if look.audio_enabled { "传回本机：已开启" } else { "传回本机：已关闭" };
            if look.audio_mute_host {
                format!("{main} · 对方扬声器已静音")
            } else {
                main.into()
            }
        }
        Part::AudioMenu => "声音设置：传回音量、对方扬声器、音质、声道".into(),
        Part::Terminal => "终端：在对方电脑上开 PowerShell".into(),
        Part::Full if look.full => "退出全屏".into(),
        Part::Full => "全屏".into(),
    })
}

fn has_tip(part: Part) -> bool {
    !matches!(part, Part::Badge | Part::Disconnect)
}

fn tone_color(t: Tone) -> u32 {
    match t {
        Tone::Good => GREEN,
        Tone::Fair => AMBER,
        Tone::Bad => RED,
    }
}

/// The controls left to right from `x0`, centred in a strip `h` pixels high.
struct Layout {
    parts: Vec<(Part, RECT)>,
    seps: Vec<i32>,
    note: Option<RECT>,
    right: i32,
}

fn layout(px: &dyn Fn(i32) -> i32, m: &Metrics, h: i32, x0: i32, badge_w: i32, note_w: i32, look: &Look) -> Layout {
    let span = |size: i32| ((h - px(size)) / 2, (h + px(size)) / 2);
    let (mut parts, mut seps) = (Vec::new(), Vec::new());
    let mut x = x0;
    let (t, b) = span(m.badge_h);
    let bw = px(22) + badge_w + px(8);
    parts.push((Part::Badge, rect(x, t, x + bw, b)));
    x += bw + px(5);
    seps.push(x);
    x += px(6);
    let mut icons = Vec::new();
    if look.monitors > 1 {
        icons.push(Part::Monitor);
    }
    if look.input {
        icons.push(Part::Cad);
    }
    icons.extend([Part::Quality, Part::Audio, Part::AudioMenu, Part::Terminal, Part::Full]);
    let (t, b) = span(m.icon_h);
    for p in icons {
        let w = if p == Part::AudioMenu { px(16) } else { px(m.icon_w) };
        let gap = if p == Part::Audio { 0 } else { px(2) };
        parts.push((p, rect(x, t, x + w, b)));
        x += w + gap;
    }
    x += px(3);
    seps.push(x);
    x += px(6);
    let (t, b) = span(m.end_h);
    parts.push((Part::Disconnect, rect(x, t, x + px(m.end_w), b)));
    x += px(m.end_w);
    let note = look.released.then(|| {
        let r = rect(x + px(10), 0, x + px(10) + note_w + 2, h);
        x = r.right;
        r
    });
    Layout { parts, seps, note, right: x }
}

impl Layout {
    fn shift(&mut self, dx: i32) {
        for (_, r) in self.parts.iter_mut() {
            r.left += dx;
            r.right += dx;
        }
        for s in self.seps.iter_mut() {
            *s += dx;
        }
        if let Some(r) = self.note.as_mut() {
            r.left += dx;
            r.right += dx;
        }
        self.right += dx;
    }
}

fn moved(r: &RECT, dx: i32, dy: i32) -> RECT {
    rect(r.left + dx, r.top + dy, r.right + dx, r.bottom + dy)
}

struct Paint<'a> {
    px: &'a dyn Fn(i32) -> i32,
    pf: &'a dyn Fn(f32) -> f32,
    font: HFONT,
    hover: Option<Hot>,
    pressed: Option<Hot>,
    stats: bool,
    menu: bool,
    audio_menu: bool,
    /// The controls react (the bar is down).
    live: bool,
}

/// The controls, `dy` pixels down from where `lay` has them.
fn paint_parts(c: &Canvas, lay: &Layout, dy: i32, h: i32, m: &Metrics, look: &Look, p: &Paint) {
    let px = p.px;
    for (part, r) in &lay.parts {
        let r = moved(r, 0, dy);
        let hot = p.live && p.hover == Some(Hot::Part(*part));
        let down = hot && p.pressed == Some(Hot::Part(*part));
        match part {
            Part::Badge => {
                if hot || p.stats {
                    c.round(&r, px(6), Some(rgb(theme::BUTTON_HOVER)), None);
                }
                let cy = (r.top + r.bottom) / 2;
                c.circle(r.left + px(12), cy, px(4), Some(rgb(tone_color(look.tone))), None);
                c.text(&rect(r.left + px(22), r.top, r.right, r.bottom), &look.badge, p.font, gdi::TEXT, false);
            }
            Part::Disconnect => {
                let fill = match (hot, down) {
                    (true, true) => 0x4a2b2c,
                    (true, false) => 0x5a2f30,
                    _ => theme::BUTTON_HOVER,
                };
                c.round(&r, px(6), Some(rgb(fill)), None);
                c.text(&r, "断开", p.font, rgb(DANGER), true);
            }
            _ => {
                let open = (*part == Part::Quality && p.menu) || (*part == Part::AudioMenu && p.audio_menu);
                if open && !hot {
                    c.round(&r, px(6), Some(rgb(theme::BUTTON_HOVER)), None);
                }
                if hot {
                    c.round(&r, px(6), Some(rgb(if down { theme::BUTTON } else { theme::BUTTON_HOVER })), None);
                }
                let color = if hot || open { theme::TEXT } else { theme::DIM };
                icon(c, *part, &r, p.pf, rgb(color), look);
            }
        }
    }
    let cy = h / 2 + dy;
    for s in &lay.seps {
        c.fill(&rect(*s, cy - px(m.sep_h) / 2, *s + 1, cy + px(m.sep_h) / 2), rgb(EDGE));
    }
    if let Some(n) = lay.note {
        c.text(&moved(&n, 0, dy), RELEASED, p.font, rgb(AMBER), false);
    }
}

/// The title bar of the session window `hwnd` (its WM_PAINT).
pub fn paint_caption(hwnd: HWND) {
    let px = gdi::scale(hwnd);
    let pf = |v: f32| v * px(96) as f32 / 96.0;
    let h = px(CAPTION);
    let mut c = Canvas::begin_top(hwnd, rgb(FILL), h);
    let look = look();
    if look.full {
        return;
    }
    let bold = c.font(px(IN_CAPTION.font), true);
    let f = c.font(px(IN_CAPTION.font), false);
    let name = window::name();
    let name_w = c.text_width(&name, bold);
    let badge_w = c.text_width(&look.badge, f);
    let note_w = c.text_width(RELEASED, f);
    let active = unsafe { GetForegroundWindow() } == hwnd;
    let left = px(12);
    let w = c.size.right;
    let cw = px(CAP_W);
    let mut lay = layout(&px, &IN_CAPTION, h, 0, badge_w, note_w, &look);
    // Centred like the full-screen bar, the released note hanging off the
    // right; a narrow window moves them left of the caption buttons, never
    // overlapping the caption buttons (name truncates with ellipsis).
    let controls_w = lay.parts.last().map_or(0, |(_, r)| r.right);
    let dx = ((w - controls_w) / 2).max(left + name_w + px(14)).min(w - cw * 3 - px(8) - lay.right);
    lay.shift(dx);
    let controls_left = lay.parts.first().map_or(w - cw * 3 - px(8), |(_, r)| r.left);
    let name_max_w = (controls_left - left - px(12)).max(px(32));
    c.text(&rect(left, 0, left + (name_w + 2).min(name_max_w), h), &name, bold, if active { gdi::TEXT } else { gdi::DIM }, false);
    let (hover, pressed, stats, menu, audio_menu) =
        with(|s| (s.hover.map(|h| h.0), s.pressed, s.stats, s.menu, s.audio_menu)).unwrap_or((None, None, false, false, false));
    let paint = Paint { px: &px, pf: &pf, font: f, hover, pressed, stats, menu, audio_menu, live: true };
    paint_parts(&c, &lay, 0, h, &IN_CAPTION, &look, &paint);

    let caps = [(Hot::Min, rect(w - cw * 3, 0, w - cw * 2, h)), (Hot::Max, rect(w - cw * 2, 0, w - cw, h)), (Hot::Close, rect(w - cw, 0, w, h))];
    let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
    for (k, r) in &caps {
        // The caption buttons win over controls a narrow window pushes under them.
        c.fill(r, rgb(FILL));
        let hot = hover == Some(*k);
        paint_cap(&c, *k, r, hot, hot && pressed == Some(*k), zoomed, active, &pf);
    }
    let mut hits: Vec<(Hot, RECT)> = caps.to_vec();
    hits.extend(lay.parts.iter().filter(|(_, r)| r.right <= w - cw * 3).map(|(p, r)| (Hot::Part(*p), *r)));
    let need_follow = with(|s| {
        let changed = s.caption != hits;
        s.caption = hits;
        changed && (s.stats || s.menu || s.audio_menu || s.tip_shown)
    })
    .unwrap_or(false);
    if need_follow {
        post(WM_APP_FOLLOW);
    }
}

/// A caption button with its glyph, drawn like Windows 11's.
#[allow(clippy::too_many_arguments)]
fn paint_cap(c: &Canvas, k: Hot, r: &RECT, hot: bool, down: bool, zoomed: bool, active: bool, pf: &dyn Fn(f32) -> f32) {
    let close = k == Hot::Close;
    if hot {
        let bg = match (close, down) {
            (true, false) => CLOSE_HOT,
            (true, true) => CLOSE_DOWN,
            (false, false) => theme::BUTTON_HOVER,
            (false, true) => theme::BUTTON,
        };
        c.fill(r, rgb(bg));
    }
    let color = rgb(if hot && close {
        0xffffff
    } else if active || hot {
        theme::TEXT
    } else {
        theme::DIM
    });
    let (cx, cy) = (((r.left + r.right) / 2) as f32 + 0.5, ((r.top + r.bottom) / 2) as f32 + 0.5);
    let s = pf(5.0).round();
    let w = pf(1.0);
    match k {
        Hot::Min => c.lines(&[(cx - s, cy), (cx + s, cy)], w, color),
        Hot::Max if zoomed => {
            let o = pf(2.0).round();
            c.lines(&[(cx - s, cy - s + o), (cx + s - o, cy - s + o), (cx + s - o, cy + s), (cx - s, cy + s), (cx - s, cy - s + o)], w, color);
            c.lines(&[(cx - s + o, cy - s + o), (cx - s + o, cy - s), (cx + s, cy - s), (cx + s, cy + s - o), (cx + s - o, cy + s - o)], w, color);
        }
        Hot::Max => c.lines(&[(cx - s, cy - s), (cx + s, cy - s), (cx + s, cy + s), (cx - s, cy + s), (cx - s, cy - s)], w, color),
        Hot::Close => {
            c.lines(&[(cx - s, cy - s), (cx + s, cy + s)], w, color);
            c.lines(&[(cx + s, cy - s), (cx - s, cy + s)], w, color);
        }
        Hot::Part(_) => {}
    }
}

fn caption_at(x: i32, y: i32) -> Option<Hot> {
    with(|s| s.caption.iter().find(|(_, r)| gdi::inside(r, x, y)).map(|(k, _)| *k)).flatten()
}

/// What the title bar has at (`x`, `y`) in client pixels, for WM_NCHITTEST:
/// the controls are client area, the rest drags the window.
pub fn caption_hit(x: i32, y: i32) -> u32 {
    match caption_at(x, y) {
        Some(Hot::Part(_)) => HTCLIENT,
        Some(Hot::Min) => HTMINBUTTON,
        Some(Hot::Max) => HTMAXBUTTON,
        Some(Hot::Close) => HTCLOSE,
        None => HTCAPTION,
    }
}

fn cap_of(ht: usize) -> Option<Hot> {
    match ht as u32 {
        HTMINBUTTON => Some(Hot::Min),
        HTMAXBUTTON => Some(Hot::Max),
        HTCLOSE => Some(Hot::Close),
        _ => None,
    }
}

fn set_hover(owner: HWND, k: Option<Hot>) {
    let changed = with(|s| {
        if s.hover.map(|h| h.0) == k {
            return false;
        }
        s.hover = k.map(|k| (k, Instant::now()));
        s.pressed = s.pressed.filter(|p| Some(*p) == k);
        s.r_pressed = s.r_pressed.filter(|p| Some(*p) == k);
        true
    })
    .unwrap_or(false);
    if changed {
        invalidate_caption(owner);
        redraw(toolbar());
    }
}

fn track(hwnd: HWND, nonclient: bool) {
    let flags = if nonclient { TME_LEAVE | TME_NONCLIENT } else { TME_LEAVE };
    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: flags, hwndTrack: hwnd, dwHoverTime: 0 };
    unsafe {
        let _ = TrackMouseEvent(&mut t);
    }
}

/// The title bar's mouse messages; None to leave the message to Windows
/// (dragging, the window menu).
pub fn caption_mouse(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let point = || POINT { x: lp.0 as i16 as i32, y: (lp.0 >> 16) as i16 as i32 };
    let client_at = || {
        let p = point();
        caption_at(p.x, p.y)
    };
    match msg {
        WM_MOUSEMOVE => {
            set_hover(hwnd, client_at().filter(|k| matches!(k, Hot::Part(_))));
            track(hwnd, false);
            Some(LRESULT(0))
        }
        WM_NCMOUSEMOVE => {
            set_hover(hwnd, cap_of(wp.0));
            track(hwnd, true);
            // Windows would draw its old buttons over the caption buttons.
            cap_of(wp.0).map(|_| LRESULT(0))
        }
        WM_MOUSELEAVE | WM_NCMOUSELEAVE => {
            // Leaving the client area for the caption buttons (or back) sends
            // a leave too: what is under the pointer now decides.
            let mut p = POINT::default();
            let mut k = None;
            unsafe {
                if GetCursorPos(&mut p).is_ok() && WindowFromPoint(p) == hwnd {
                    let _ = ScreenToClient(hwnd, &mut p);
                    k = caption_at(p.x, p.y);
                }
            }
            set_hover(hwnd, k);
            if let Some(k) = k {
                track(hwnd, !matches!(k, Hot::Part(_)));
            }
            Some(LRESULT(0))
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let k = client_at();
            with(|s| s.pressed = k);
            invalidate_caption(hwnd);
            Some(LRESULT(0))
        }
        WM_LBUTTONUP => {
            let k = client_at();
            let was = with(|s| s.pressed.take()).flatten();
            invalidate_caption(hwnd);
            if let (Some(Hot::Part(p)), true) = (was, was == k) {
                act(p);
            }
            Some(LRESULT(0))
        }
        WM_RBUTTONDOWN => {
            let k = client_at();
            if k == Some(Hot::Part(Part::Audio)) {
                with(|s| s.r_pressed = k);
                return Some(LRESULT(0));
            }
            None
        }
        WM_RBUTTONUP => {
            let k = client_at();
            let was = with(|s| s.r_pressed.take()).flatten();
            if was == Some(Hot::Part(Part::Audio)) && k == was {
                toggle_audio_menu();
                return Some(LRESULT(0));
            }
            None
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
            let k = cap_of(wp.0)?;
            with(|s| s.pressed = Some(k));
            invalidate_caption(hwnd);
            Some(LRESULT(0))
        }
        WM_NCLBUTTONUP => {
            let k = cap_of(wp.0)?;
            let was = with(|s| s.pressed.take()).flatten();
            invalidate_caption(hwnd);
            if was == Some(k) {
                let cmd = match k {
                    Hot::Min => SC_MINIMIZE,
                    Hot::Max if unsafe { IsZoomed(hwnd) }.as_bool() => SC_RESTORE,
                    Hot::Max => SC_MAXIMIZE,
                    _ => SC_CLOSE,
                };
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
                }
            }
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// Measures with the controls' font: the badge, the released note, the tip
/// (if one is due) and the numbers' widest key and value.
fn measure(px: &dyn Fn(i32) -> i32, m: &Metrics, look: &Look, tip_text: Option<&str>) -> (i32, i32, i32, i32, i32) {
    let mut c = Canvas::offscreen(1, 1, rgb(FILL));
    let f = c.font(px(m.font), false);
    let tf = c.font(px(13), false);
    let key_w = look.rows.iter().map(|r| c.text_width(&r.0, tf)).max().unwrap_or(0);
    let value_w = look.rows.iter().map(|r| c.text_width(&r.1, tf)).max().unwrap_or(0);
    (c.text_width(&look.badge, f), c.text_width(RELEASED, f), tip_text.map_or(0, |t| c.text_width(t, tf)), key_w, value_w)
}

fn due_tip(st: &State, look: &Look) -> Option<(Part, String)> {
    match st.hover {
        Some((Hot::Part(p), since)) if since.elapsed() >= TIP_DELAY && !st.stats && !st.menu && !st.audio_menu => {
            tip(p, look).map(|t| (p, t))
        }
        _ => None,
    }
}

fn panel_size(px: &dyn Fn(i32) -> i32, look: &Look, key_w: i32, value_w: i32) -> (i32, i32) {
    (px(STATS_W).max(key_w + value_w + px(28 + 24)), px(12) * 2 + px(ROW_H) * look.rows.len() as i32)
}

fn paint_panel(c: &Canvas, r: &RECT, px: &dyn Fn(i32) -> i32, f: HFONT, look: &Look, value_w: i32) {
    c.round(r, px(10), Some(rgb(FILL)), Some((rgb(EDGE), 1)));
    for (i, (k, v)) in look.rows.iter().enumerate() {
        let y = r.top + px(12) + px(ROW_H) * i as i32;
        let row = rect(r.left + px(14), y, r.right - px(14), y + px(ROW_H));
        c.text(&row, k, f, gdi::DIM, false);
        c.text(&rect(row.right - value_w - 2, y, row.right, y + px(ROW_H)), v, f, gdi::TEXT, false);
    }
}

struct Slider {
    hit: RECT,
    track: RECT,
    value: RECT,
}

/// The 画面 or 声音 menu, laid out from (0, 0).
struct Menu {
    size: (i32, i32),
    labels: Vec<(&'static str, RECT)>,
    slider: Option<Slider>,
    chips: Vec<(Chip, String, RECT)>,
    line: i32,
    footer: (String, RECT),
}

fn mbps_text(kbps: u32) -> String {
    let m = kbps as f64 / 1000.0;
    if m < 10.0 { format!("{m:.1}") } else { format!("{m:.0}") }
}

/// The menu's last line: what the host streams now.
fn menu_footer() -> String {
    match quality::streaming() {
        None => "等对方发来画面…".into(),
        Some((_, None)) => "对方的软件较旧，更新后这些设置才生效".into(),
        Some(((w, h), Some(s))) => format!(
            "现在 {w}×{h} · {} 帧/秒 · 最高 {} Mbps{}",
            s.fps,
            mbps_text(s.max_kbps),
            if s.yuv444 { " · 文字清晰" } else { "" }
        ),
    }
}

fn menu_layout(px: &dyn Fn(i32) -> i32) -> Menu {
    let mut rows: Vec<(&'static str, Vec<(Chip, String)>)> = vec![
        ("分辨率", quality::RESOLUTIONS.iter().map(|(r, t)| (Chip::Res(*r), t.to_string())).collect()),
        ("帧率", quality::FRAME_RATES.iter().map(|&v| (Chip::Fps(v), if v == 0 { "自动".into() } else { v.to_string() })).collect()),
        (
            "码率",
            quality::BITRATES.iter().map(|&v| (Chip::Mbps(v), if v == 0 { "自动".into() } else { format!("{v}M") })).collect(),
        ),
    ];
    // Only when both computers can (YUV 4:4:4: HEVC on the graphics cards of both).
    if matches!(quality::streaming(), Some((_, Some(s))) if s.can444) {
        rows.push(("文字", vec![(Chip::Sharp(false), "标准".into()), (Chip::Sharp(true), "清晰".into())]));
    }
    build_menu(px, None, rows, menu_footer())
}

fn audio_menu_footer() -> String {
    let c = audio::config::choice();
    if !c.enabled {
        return if c.mute_host {
            "未传回本机 · 对方已静音".into()
        } else {
            "未传回本机".into()
        };
    }
    let ch = if c.channels == 1 { "单声道" } else { "立体声" };
    let rate_hz = audio::config::active_rate_hz();
    let rate_str = if rate_hz.is_multiple_of(1000) {
        format!("{} kHz", rate_hz / 1000)
    } else {
        format!("{:.1} kHz", rate_hz as f32 / 1000.0)
    };
    let q = if c.lossless { "无损 PCM" } else { "Opus" };
    let mute = if c.mute_host { " · 对方已静音" } else { "" };
    format!("{q} · {ch} · {rate_str}{mute}")
}

fn audio_menu_layout(px: &dyn Fn(i32) -> i32) -> Menu {
    let rows = vec![
        ("对方扬声器", vec![(Chip::AudioMuteHost(false), "正常出声".into()), (Chip::AudioMuteHost(true), "静音".into())]),
        ("音质", vec![(Chip::AudioLossless(false), "标准".into()), (Chip::AudioLossless(true), "无损".into())]),
        ("声道", vec![(Chip::AudioCh(2), "立体声".into()), (Chip::AudioCh(1), "单声道".into())]),
    ];
    build_menu(px, Some("传回音量"), rows, audio_menu_footer())
}

fn build_menu(
    px: &dyn Fn(i32) -> i32,
    slider_label: Option<&'static str>,
    rows: Vec<(&'static str, Vec<(Chip, String)>)>,
    footer_text: String,
) -> Menu {
    let mut c = Canvas::offscreen(1, 1, rgb(FILL));
    let f = c.font(px(13), false);
    let pad = px(14);
    let max_label_w = rows
        .iter()
        .map(|(l, _)| *l)
        .chain(slider_label)
        .map(|l| c.text_width(l, f))
        .max()
        .unwrap_or(0);
    let label_w = px(MENU_LABEL_W).max(max_label_w + px(14));
    let x0 = pad + label_w;
    let (mut labels, mut chips) = (Vec::new(), Vec::new());
    let mut y = px(8);
    let mut right = 0;
    let slider_y = slider_label.map(|label| {
        let sy = y;
        labels.push((label, rect(pad, sy, x0, sy + px(MENU_ROW))));
        y += px(MENU_ROW);
        sy
    });
    for (label, row) in rows {
        labels.push((label, rect(pad, y, x0, y + px(MENU_ROW))));
        let mut x = x0;
        let top = y + (px(MENU_ROW) - px(CHIP_H)) / 2;
        for (chip, text) in row {
            let w = c.text_width(&text, f) + px(20);
            chips.push((chip, text, rect(x, top, x + w, top + px(CHIP_H))));
            x += w + px(6);
        }
        right = right.max(x - px(6));
        y += px(MENU_ROW);
    }
    y += px(6);
    let line = y;
    y += px(6);
    let fw = c.text_width(&footer_text, f);
    let footer = (footer_text, rect(pad, y, pad + fw + 2, y + px(ROW_H)));
    y += px(ROW_H) + px(8);
    let val_w = c.text_width("100%", f) + px(4);
    let min_slider_right = if slider_y.is_some() { x0 + px(120) + px(10) + val_w } else { 0 };
    let content_right = right.max(pad + fw + 2).max(min_slider_right);
    let slider = slider_y.map(|sy| {
        let cy = sy + px(MENU_ROW) / 2;
        let track_l = x0 + px(6);
        let track_r = (content_right - val_w - px(10)).max(track_l + px(40));
        let half_h = px(2).max(1);
        Slider {
            hit: rect(track_l - px(6), sy + px(3), track_r + px(6), sy + px(MENU_ROW) - px(3)),
            track: rect(track_l, cy - half_h, track_r, cy + half_h),
            value: rect(content_right - val_w, sy, content_right + 2, sy + px(MENU_ROW)),
        }
    });
    let w = content_right + pad;
    Menu { size: (w, y), labels, slider, chips, line, footer }
}

/// Paints the menu at `at` (its top left in `c`); returns its chips and optional slider there.
fn paint_menu(
    c: &Canvas,
    m: &Menu,
    at: POINT,
    px: &dyn Fn(i32) -> i32,
    f: HFONT,
    st: &State,
) -> (Vec<(Chip, RECT)>, Option<SliderRects>) {
    let (dx, dy) = (at.x, at.y);
    let r = rect(dx, dy, dx + m.size.0, dy + m.size.1);
    c.round(&r, px(10), Some(rgb(FILL)), Some((rgb(EDGE), 1)));
    for (label, lr) in &m.labels {
        c.text(&moved(lr, dx, dy), label, f, gdi::DIM, false);
    }
    let slider_hit = m.slider.as_ref().map(|s| {
        let vol = audio::config::display_volume();
        let hit = moved(&s.hit, dx, dy);
        let tr = moved(&s.track, dx, dy);
        let vr = moved(&s.value, dx, dy);
        let cy = (tr.top + tr.bottom) / 2;
        let fill_x = tr.left + ((tr.right - tr.left) * vol as i32 + 50) / 100;
        c.round(&tr, px(2), Some(rgb(theme::BUTTON_EDGE)), None);
        if fill_x > tr.left {
            c.round(&rect(tr.left, tr.top, fill_x, tr.bottom), px(2), Some(rgb(theme::ACCENT)), None);
        }
        let active = st.slider_drag || st.slider_hover;
        let knob_r = if active { px(7) } else { px(6) };
        let knob_fill = if st.slider_drag {
            rgb(theme::ACCENT_HOVER)
        } else if vol > 0 || active {
            rgb(0xffffff)
        } else {
            gdi::DIM
        };
        c.circle(fill_x, cy, knob_r, Some(knob_fill), Some((rgb(FILL), 1)));
        c.text(&vr, &format!("{vol}%"), f, if vol > 0 { gdi::TEXT } else { gdi::DIM }, false);
        SliderRects { hit, track: tr }
    });
    let q_choice = quality::choice();
    let a_choice = audio::config::choice();
    let mut hits = Vec::new();
    for (chip, text, cr) in &m.chips {
        let cr = moved(cr, dx, dy);
        let on = chip.on(&q_choice, &a_choice);
        let hot = st.chip_hover == Some(*chip);
        let down = hot && st.chip_pressed == Some(*chip);
        let fill = match (on, hot, down) {
            (true, _, true) => theme::ACCENT_PRESSED,
            (true, true, _) => theme::ACCENT_HOVER,
            (true, false, _) => theme::ACCENT,
            (false, true, false) => theme::BUTTON_HOVER,
            _ => theme::BUTTON,
        };
        c.round(&cr, px(6), Some(rgb(fill)), if on { None } else { Some((rgb(theme::BUTTON_EDGE), 1)) });
        c.text(&cr, text, f, if on { rgb(0xffffff) } else { gdi::TEXT }, true);
        hits.push((*chip, cr));
    }
    c.fill(&rect(dx + px(14), dy + m.line, dx + m.size.0 - px(14), dy + m.line + 1), rgb(EDGE));
    c.text(&moved(&m.footer.1, dx, dy), &m.footer.0, f, gdi::DIM, false);
    (hits, slider_hit)
}

fn paint_tip(c: &Canvas, r: &RECT, px: &dyn Fn(i32) -> i32, f: HFONT, text: &str) {
    c.round(r, px(6), Some(rgb(theme::RAISED)), Some((rgb(EDGE), 1)));
    c.text(r, text, f, gdi::TEXT, true);
}

fn shape(r: &RECT, radius: i32) -> ([f32; 4], f32) {
    ([r.left, r.top, r.right, r.bottom].map(|v| v as f32), radius as f32)
}

fn present(c: &Canvas, hwnd: HWND, at: POINT, shapes: &[([f32; 4], f32)]) {
    c.present_layered(hwnd, at.x, at.y, |x, y| {
        let mut a = 0.0f32;
        for (s, rr) in shapes {
            let v = coverage(*s, *rr, x, y);
            a = a + v * (1.0 - a);
        }
        a
    });
}

fn draw(hwnd: HWND, st: &mut State) {
    let look = look();
    if st.stats && look.rows.is_empty() {
        st.stats = false;
    }
    if look.full != st.full {
        st.full = look.full;
        st.hover = None;
        st.pressed = None;
        st.r_pressed = None;
        if look.full {
            let now = Instant::now();
            st.started = now;
            st.last_near = now;
            st.open = true;
            st.t = 1.0;
        }
    }
    if look.full {
        draw_bar(hwnd, st, &look);
        if (look.hold || look.released) && !st.open {
            st.open = true;
            start_animation(hwnd, st);
        }
    } else {
        draw_popups(hwnd, st, &look);
    }
}

fn show(hwnd: HWND, on: bool) {
    unsafe {
        if IsWindowVisible(hwnd).as_bool() != on {
            let _ = ShowWindow(hwnd, if on { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}

fn work_area(owner: HWND) -> RECT {
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let ok = unsafe { GetMonitorInfoW(MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST), &mut mi) }.as_bool();
    if ok {
        mi.rcWork
    } else {
        let mut r = RECT::default();
        unsafe {
            let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut r as *mut RECT as *mut _), Default::default());
        }
        r
    }
}

fn clamp_rect(r: RECT, bounds: &RECT, pad: i32) -> RECT {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let min_x = bounds.left + pad;
    let max_x = (bounds.right - pad - w).max(min_x);
    let min_y = bounds.top + pad;
    let max_y = (bounds.bottom - pad - h).max(min_y);
    let x = r.left.clamp(min_x, max_x);
    let y = r.top.clamp(min_y, max_y);
    rect(x, y, x + w, y + h)
}

/// In a window: the numbers under the badge, a tip under a title bar button.
fn draw_popups(hwnd: HWND, st: &mut State, look: &Look) {
    let px = gdi::scale(st.owner);
    let tip_due = due_tip(st, look);
    let (_, _, tip_w, key_w, value_w) = measure(&px, &IN_CAPTION, look, tip_due.as_ref().map(|t| t.1.as_str()));
    let owner = st.owner;
    let work = work_area(owner);
    let screen = |r: &RECT| {
        let mut p = POINT { x: r.left, y: r.top };
        unsafe {
            let _ = ClientToScreen(owner, &mut p);
        }
        rect(p.x, p.y, p.x + r.right - r.left, p.y + r.bottom - r.top)
    };
    let find = |k: Hot| st.caption.iter().find(|(h, _)| *h == k).map(|(_, r)| screen(r));
    let panel = match find(Hot::Part(Part::Badge)) {
        Some(b) if st.stats && !look.rows.is_empty() => {
            let (w, h) = panel_size(&px, look, key_w, value_w);
            let top = b.bottom + px(6);
            Some(clamp_rect(rect(b.left - px(6), top, b.left - px(6) + w, top + h), &work, px(6)))
        }
        _ => None,
    };
    let menu = if st.menu {
        find(Hot::Part(Part::Quality)).map(|b| (b, menu_layout(&px)))
    } else if st.audio_menu {
        find(Hot::Part(Part::Audio)).map(|b| (b, audio_menu_layout(&px)))
    } else {
        None
    };
    let menu = menu.map(|(b, m)| {
        let (w, h) = m.size;
        // Under the button, kept inside the session window where it can be, then clamped to monitor work area.
        let (origin, client_w) = owner_top(owner);
        let l = (b.left - px(6)).min(origin.x + client_w - w - px(6)).max(origin.x + px(6));
        let r = clamp_rect(rect(l, b.bottom + px(6), l + w, b.bottom + px(6) + h), &work, px(6));
        (r, m)
    });
    let tip = tip_due.and_then(|(p, text)| {
        let r = find(Hot::Part(p))?;
        let w = tip_w + px(16);
        let l = (r.left + r.right) / 2 - w / 2;
        let tr = clamp_rect(rect(l, r.bottom + px(6), l + w, r.bottom + px(6) + px(24)), &work, px(6));
        Some((tr, text))
    });
    st.tip_shown = tip.is_some();
    st.parts.clear();
    st.panel = None;
    st.chips.clear();
    st.slider = None;
    let boxes: Vec<RECT> = panel.iter().copied().chain(menu.iter().map(|m| m.0)).chain(tip.iter().map(|t| t.0)).collect();
    if boxes.is_empty() {
        show(hwnd, false);
        return;
    }
    let area = boxes.iter().skip(1).fold(boxes[0], |a, b| rect(a.left.min(b.left), a.top.min(b.top), a.right.max(b.right), a.bottom.max(b.bottom)));
    let at = POINT { x: area.left - 1, y: area.top - 1 };
    let mut c = Canvas::offscreen(area.right - area.left + 2, area.bottom - area.top + 2, rgb(FILL));
    let f = c.font(px(13), false);
    let mut shapes = Vec::new();
    if let Some(p) = panel {
        let r = moved(&p, -at.x, -at.y);
        paint_panel(&c, &r, &px, f, look, value_w);
        shapes.push(shape(&r, px(10)));
    }
    if let Some((r, m)) = &menu {
        let r = moved(r, -at.x, -at.y);
        let (chips, slider) = paint_menu(&c, m, POINT { x: r.left, y: r.top }, &px, f, st);
        st.chips = chips;
        st.slider = slider;
        shapes.push(shape(&r, px(10)));
    }
    if let Some((t, text)) = &tip {
        let r = moved(t, -at.x, -at.y);
        paint_tip(&c, &r, &px, f, text);
        shapes.push(shape(&r, px(6)));
    }
    present(&c, hwnd, at, &shapes);
    st.origin = at;
    show(hwnd, true);
}

/// Full screen: the bar at the top edge, with the numbers or a tip under it.
fn draw_bar(hwnd: HWND, st: &mut State, look: &Look) {
    let px = gdi::scale(st.owner);
    let pf = |v: f32| v * px(96) as f32 / 96.0;
    let tip_due = due_tip(st, look);
    let (badge_w, note_w, tip_w, key_w, value_w) = measure(&px, &IN_BAR, look, tip_due.as_ref().map(|t| t.1.as_str()));
    st.tip_shown = tip_due.is_some();

    let h = px(BAR_H);
    let mut lay = layout(&px, &IN_BAR, h, px(6), badge_w, note_w, look);
    let bar_w = lay.right + if lay.note.is_some() { px(12) } else { px(6) };
    let menu = if st.menu {
        Some(menu_layout(&px))
    } else if st.audio_menu {
        Some(audio_menu_layout(&px))
    } else {
        None
    };
    let show_stats = st.stats && !look.rows.is_empty();
    let (panel_w, panel_h) = match &menu {
        Some(m) => m.size,
        None if show_stats => panel_size(&px, look, key_w, value_w),
        None => (0, 0),
    };
    let panel_h = if show_stats || st.menu || st.audio_menu { panel_h } else { 0 };
    let win_w = bar_w.max(panel_w).max(tip_w + px(40)) + 2;
    let win_h = h + px(8) + panel_h.max(px(30)) + 2;
    let left = (win_w - bar_w) / 2;
    lay.shift(left);

    let off = ((1.0 - eased(st.t)) * (h - px(HANDLE)) as f32).round() as i32;
    let mut c = Canvas::offscreen(win_w, win_h, rgb(FILL));
    let f = c.font(px(IN_BAR.font), false);
    let r = px(RADIUS);
    let bar = rect(left, -off - r, left + bar_w, h - off);
    c.round(&bar, r, Some(rgb(FILL)), Some((rgb(EDGE), 1)));
    // A grip on the edge that is left when the bar is up.
    let grip = 1.0 - eased(st.t);
    if grip > 0.01 {
        let gy = (h - off - px(2)) as f32;
        let cx = (left + bar_w / 2) as f32;
        c.lines(&[(cx - pf(14.0), gy), (cx + pf(14.0), gy)], pf(1.5), rgb(mix(FILL, theme::DIM, grip)));
    }
    let paint = Paint {
        px: &px,
        pf: &pf,
        font: f,
        hover: st.hover.map(|h| h.0),
        pressed: st.pressed,
        stats: st.stats,
        menu: st.menu,
        audio_menu: st.audio_menu,
        live: st.t > 0.9,
    };
    paint_parts(&c, &lay, -off, h, &IN_BAR, look, &paint);

    let mut shapes = vec![shape(&bar, r)];
    let mut panel = None;
    st.chips.clear();
    st.slider = None;
    if (show_stats || st.menu || st.audio_menu) && st.t > 0.0 {
        let pl = (win_w - panel_w) / 2;
        let pt = h - off + px(8);
        let pr = rect(pl, pt, pl + panel_w, pt + panel_h);
        match &menu {
            Some(m) => {
                let (chips, slider) = paint_menu(&c, m, POINT { x: pl, y: pt }, &px, f, st);
                st.chips = chips;
                st.slider = slider;
            }
            None => paint_panel(&c, &pr, &px, f, look, value_w),
        }
        shapes.push(shape(&pr, px(10)));
        panel = Some(pr);
    }
    if let Some((p, text)) = &tip_due
        && let Some((_, br)) = lay.parts.iter().find(|(q, _)| q == p)
    {
        let tw = tip_w + px(16);
        let tl = ((br.left + br.right) / 2 - tw / 2).clamp(1, win_w - tw - 1);
        let tt = h - off + px(6);
        let tr = rect(tl, tt, tl + tw, tt + px(24));
        paint_tip(&c, &tr, &px, f, text);
        shapes.push(shape(&tr, px(6)));
    }

    let (origin, client_w) = owner_top(st.owner);
    let pos = POINT { x: origin.x + (client_w - win_w) / 2, y: origin.y };
    present(&c, hwnd, pos, &shapes);
    st.parts = lay.parts.iter().map(|(p, r)| (*p, moved(r, 0, -off))).collect();
    st.bar = rect(left, 0, left + bar_w, h - off);
    st.panel = panel;
    st.origin = pos;
    show(hwnd, true);
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = ((a >> s & 0xff) as f32, (b >> s & 0xff) as f32);
        ((x + (y - x) * t).round() as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}

/// The session window's client area: its top left on the screen, and width.
fn owner_top(owner: HWND) -> (POINT, i32) {
    let mut p = POINT::default();
    let mut c = RECT::default();
    unsafe {
        let _ = ClientToScreen(owner, &mut p);
        let _ = GetClientRect(owner, &mut c);
    }
    (p, c.right)
}

/// Icons on a 16 × 16 grid, centred in `r`.
fn icon(c: &Canvas, part: Part, r: &RECT, pf: &dyn Fn(f32) -> f32, color: COLORREF, look: &Look) {
    let s = pf(16.0) / 16.0;
    let (ox, oy) = ((r.left + r.right) as f32 / 2.0 - 8.0 * s, (r.top + r.bottom) as f32 / 2.0 - 8.0 * s);
    let p = |x: f32, y: f32| (ox + x * s, oy + y * s);
    let w = pf(1.5);
    let line = |pts: &[(f32, f32)]| c.lines(&pts.iter().map(|&(x, y)| p(x, y)).collect::<Vec<_>>(), w, color);
    let frame = |l: f32, t: f32, rr: f32, b: f32| line(&[(l, t), (rr, t), (rr, b), (l, b), (l, t)]);
    match part {
        Part::Monitor => {
            frame(1.8, 2.8, 14.2, 10.8);
            line(&[(8.0, 10.8), (8.0, 13.5)]);
            line(&[(5.5, 13.5), (10.5, 13.5)]);
        }
        Part::Cad => {
            frame(1.5, 4.0, 14.5, 12.0);
            line(&[(4.0, 8.0), (4.1, 8.0)]);
            line(&[(6.5, 8.0), (6.6, 8.0)]);
            line(&[(9.0, 8.0), (12.0, 8.0)]);
        }
        Part::Quality => {
            // Three sliders.
            for (y, k) in [(4.0, 10.5), (8.0, 5.0), (12.0, 9.0)] {
                line(&[(1.5, y), (k - 1.8, y)]);
                line(&[(k + 1.8, y), (14.5, y)]);
                frame(k - 1.8, y - 1.8, k + 1.8, y + 1.8);
            }
        }
        Part::Audio => {
            line(&[(2.2, 6.2), (4.6, 6.2), (8.0, 3.2), (8.0, 12.8), (4.6, 9.8), (2.2, 9.8), (2.2, 6.2)]);
            if look.audio_enabled {
                line(&[(10.2, 5.8), (11.2, 8.0), (10.2, 10.2)]);
                line(&[(12.2, 3.8), (13.8, 8.0), (12.2, 12.2)]);
            } else {
                line(&[(10.2, 6.0), (13.8, 10.0)]);
                line(&[(13.8, 6.0), (10.2, 10.0)]);
            }
            if look.audio_mute_host {
                let (dx, dy) = p(13.8, 2.2);
                let dr = pf(2.4).round().max(2.0) as i32;
                c.circle(dx.round() as i32, dy.round() as i32, dr, Some(rgb(AMBER)), Some((rgb(FILL), 1)));
            }
        }
        Part::AudioMenu => {
            line(&[(5.0, 6.5), (8.0, 9.8), (11.0, 6.5)]);
        }
        Part::Terminal => {
            frame(1.5, 2.5, 14.5, 13.5);
            line(&[(4.0, 6.0), (6.5, 8.0), (4.0, 10.0)]);
            line(&[(8.0, 10.5), (11.5, 10.5)]);
        }
        Part::Full if look.full => {
            line(&[(6.0, 2.5), (6.0, 6.0), (2.5, 6.0)]);
            line(&[(10.0, 2.5), (10.0, 6.0), (13.5, 6.0)]);
            line(&[(13.5, 10.0), (10.0, 10.0), (10.0, 13.5)]);
            line(&[(2.5, 10.0), (6.0, 10.0), (6.0, 13.5)]);
        }
        Part::Full => {
            line(&[(2.5, 6.0), (2.5, 2.5), (6.0, 2.5)]);
            line(&[(10.0, 2.5), (13.5, 2.5), (13.5, 6.0)]);
            line(&[(13.5, 10.0), (13.5, 13.5), (10.0, 13.5)]);
            line(&[(6.0, 13.5), (2.5, 13.5), (2.5, 10.0)]);
        }
        _ => {}
    }
}

fn start_animation(hwnd: HWND, st: &mut State) {
    st.last_frame = Instant::now();
    unsafe {
        SetTimer(Some(hwnd), TIMER_ANIMATE, 15, None);
    }
}

/// Full screen: the pointer at the top edge above the bar, or on the bar
/// (or the numbers) while it is down.
fn near(hwnd: HWND, st: &State) -> bool {
    let px = gdi::scale(st.owner);
    let mut p = POINT::default();
    unsafe {
        if GetCursorPos(&mut p).is_err() {
            return false;
        }
        let under = WindowFromPoint(p);
        if under != st.owner && under != hwnd && under != window::picture() {
            return false;
        }
    }
    let (x, y) = (p.x - st.origin.x, p.y - st.origin.y);
    let (l, r) = (st.bar.left, st.bar.right);
    let at_edge = y >= 0 && y < px(6) && x >= l - px(40) && x < r + px(40);
    let on_bar = st.t > 0.5 && x >= l && x < r && y < st.bar.bottom + px(6);
    let on_panel = st.panel.is_some_and(|pr| gdi::inside(&pr, x, y));
    at_edge || on_bar || on_panel
}

fn watch(hwnd: HWND, st: &mut State) {
    if st.full {
        let now = Instant::now();
        if near(hwnd, st) {
            st.last_near = now;
        }
        let look = look();
        let open = look.hold
            || look.released
            || st.stats
            || st.menu
            || st.audio_menu
            || st.slider_drag
            || now - st.started < FIRST_SHOW
            || now - st.last_near < LINGER;
        if open != st.open {
            st.open = open;
            if !open {
                st.hover = None;
                st.pressed = None;
                st.r_pressed = None;
            }
            start_animation(hwnd, st);
        }
    }
    let tip_due = st.hover.is_some_and(|(k, since)| matches!(k, Hot::Part(p) if has_tip(p)) && since.elapsed() >= TIP_DELAY);
    if tip_due && !st.tip_shown && !st.stats && !st.menu && !st.audio_menu {
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

/// The full-screen bar's control at a point in this window.
fn part_at(st: &State, lp: LPARAM) -> Option<Part> {
    let (x, y) = ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    if !st.full || st.t < 0.9 {
        return None;
    }
    st.parts.iter().find(|(_, r)| gdi::inside(r, x, y)).map(|(p, _)| *p)
}

/// The 画面 menu's chip at a point in this window.
fn chip_at(st: &State, lp: LPARAM) -> Option<Chip> {
    let (x, y) = ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    st.chips.iter().find(|(_, r)| gdi::inside(r, x, y)).map(|(c, _)| *c)
}

fn slider_at(st: &State, lp: LPARAM) -> bool {
    let (x, y) = ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    st.slider.is_some_and(|sr| gdi::inside(&sr.hit, x, y))
}

fn slider_vol(track: &RECT, x: i32) -> u8 {
    let w = (track.right - track.left).max(1);
    let t = (x - track.left).clamp(0, w);
    ((t * 100 + w / 2) / w) as u8
}

fn toggle_audio_menu() {
    with(|s| {
        s.audio_menu = !s.audio_menu;
        s.stats = false;
        s.menu = false;
        s.chip_hover = None;
        s.slider = None;
        s.slider_hover = false;
        s.slider_drag = false;
    });
    changed();
}

fn act(part: Part) {
    match part {
        Part::Badge => {
            let has_rows = !look().rows.is_empty();
            with(|s| {
                s.stats = if s.stats { false } else { has_rows };
                s.menu = false;
                s.audio_menu = false;
                s.slider = None;
                s.slider_hover = false;
                s.slider_drag = false;
            });
            changed();
        }
        Part::Quality => {
            with(|s| {
                s.menu = !s.menu;
                s.stats = false;
                s.audio_menu = false;
                s.chip_hover = None;
                s.slider = None;
                s.slider_hover = false;
                s.slider_drag = false;
            });
            changed();
        }
        Part::Audio => audio::config::toggle_enabled(),
        Part::AudioMenu => toggle_audio_menu(),
        Part::Monitor => window::pick_monitor(keys::MonitorKey::Next),
        Part::Cad => window::send_sas(),
        Part::Terminal => crate::term::open_for_session(),
        Part::Full => window::toggle_fullscreen(),
        Part::Disconnect => window::disconnect(),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                with(|s| match wp.0 {
                    TIMER_ANIMATE => animate(hwnd, s),
                    TIMER_WATCH => watch(hwnd, s),
                    _ => {}
                });
                LRESULT(0)
            }
            WM_APP_UPDATE => {
                changed();
                LRESULT(0)
            }
            WM_APP_FOLLOW => {
                redraw(hwnd);
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_SETCURSOR => {
                let hand = with(|s| s.hover.is_some() && s.full || s.chip_hover.is_some() || s.slider_hover || s.slider_drag)
                    .unwrap_or(false);
                if let Ok(c) = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }) {
                    SetCursor(Some(c));
                }
                LRESULT(1)
            }
            WM_MOUSEMOVE => {
                let dragging_vol = with(|s| {
                    if s.slider_drag
                        && let Some(sr) = s.slider
                    {
                        let x = (lp.0 & 0xffff) as i16 as i32;
                        return Some(slider_vol(&sr.track, x));
                    }
                    let on = part_at(s, lp).map(Hot::Part);
                    let chip = chip_at(s, lp);
                    let s_hover = slider_at(s, lp);
                    let mut again = chip != s.chip_hover || s_hover != s.slider_hover;
                    s.chip_hover = chip;
                    s.chip_pressed = s.chip_pressed.filter(|c| Some(*c) == chip);
                    s.slider_hover = s_hover;
                    if s.full {
                        s.r_pressed = s.r_pressed.filter(|p| Some(*p) == on);
                        if on != s.hover.map(|h| h.0) {
                            s.hover = on.map(|p| (p, Instant::now()));
                            s.pressed = s.pressed.filter(|p| Some(*p) == on);
                            again = true;
                        }
                    }
                    if again {
                        draw(hwnd, s);
                    }
                    None
                })
                .flatten();
                if let Some(vol) = dragging_vol {
                    audio::config::set_volume_live(vol);
                    redraw(hwnd);
                }
                track(hwnd, false);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                with(|s| {
                    if s.slider_drag {
                        return;
                    }
                    if s.full && (s.hover.is_some() || s.pressed.is_some() || s.r_pressed.is_some())
                        || s.chip_hover.is_some()
                        || s.slider_hover
                    {
                        if s.full {
                            s.hover = None;
                            s.pressed = None;
                            s.r_pressed = None;
                        }
                        s.chip_hover = None;
                        s.chip_pressed = None;
                        s.slider_hover = false;
                        draw(hwnd, s);
                    }
                });
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let start_vol = with(|s| {
                    if slider_at(s, lp)
                        && let Some(sr) = s.slider
                    {
                        s.slider_drag = true;
                        s.slider_hover = true;
                        s.pressed = None;
                        s.chip_pressed = None;
                        let x = (lp.0 & 0xffff) as i16 as i32;
                        return Some(slider_vol(&sr.track, x));
                    }
                    s.pressed = part_at(s, lp).map(Hot::Part);
                    s.chip_pressed = chip_at(s, lp);
                    draw(hwnd, s);
                    None
                })
                .flatten();
                if let Some(vol) = start_vol {
                    SetCapture(hwnd);
                    audio::config::set_volume_live(vol);
                    redraw(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let drag_vol = with(|s| {
                    if !s.slider_drag {
                        return None;
                    }
                    s.slider_drag = false;
                    s.slider_hover = slider_at(s, lp);
                    let x = (lp.0 & 0xffff) as i16 as i32;
                    Some(s.slider.map(|sr| slider_vol(&sr.track, x)))
                })
                .flatten();
                if let Some(vol_opt) = drag_vol {
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    if let Some(vol) = vol_opt {
                        audio::config::set_volume_live(vol);
                    }
                    audio::config::commit_volume();
                    changed();
                    return LRESULT(0);
                }
                let hit = with(|s| {
                    let was = s.pressed.take();
                    let hit = was.filter(|p| part_at(s, lp).map(Hot::Part) == Some(*p));
                    let chip = s.chip_pressed.take().filter(|c| chip_at(s, lp) == Some(*c));
                    draw(hwnd, s);
                    (hit, chip)
                });
                if let Some((hit, chip)) = hit {
                    if let Some(c) = chip {
                        c.choose();
                        changed();
                    }
                    if let Some(Hot::Part(p)) = hit {
                        act(p);
                    }
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                let was_drag = with(|s| std::mem::take(&mut s.slider_drag)).unwrap_or(false);
                if was_drag {
                    audio::config::commit_volume();
                    changed();
                }
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                with(|s| {
                    s.r_pressed = part_at(s, lp).map(Hot::Part);
                });
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let hit = with(|s| {
                    let was = s.r_pressed.take();
                    was.filter(|p| part_at(s, lp).map(Hot::Part) == Some(*p))
                })
                .flatten();
                if hit == Some(Hot::Part(Part::Audio)) {
                    toggle_audio_menu();
                }
                LRESULT(0)
            }
            WM_NCDESTROY => {
                TOOLBAR.store(0, Ordering::Relaxed);
                STATE.set(None);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
