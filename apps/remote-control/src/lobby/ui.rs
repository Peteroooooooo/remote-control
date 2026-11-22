//! The lobby window (egui), drawn after `docs/devices-mockup.html`: this
//! computer and the page switch on the left, a page on the right. The
//! devices page: copying this computer's connection code, a field for the
//! other side's, then 我的电脑, 信任设备 and 临时设备 (`devices-and-relay.md`
//! 3). The settings page: grouped lines, Windows 11 style. Messages come
//! and go as toasts at the top of the page; deleting a code or a relay can
//! be taken back while its toast shows (the request goes out when the
//! toast is gone), deleting a device is asked first. It only draws
//! `backend::Snap` and calls `Backend`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontData, FontFamily, FontId, Frame, Galley, Id, Layout, Margin,
    Order, Pos2, Rect, RectAlign, RichText, Sense, Shadow, Stroke, StrokeKind, TextEdit, UiBuilder, Vec2, WidgetInfo,
    WidgetType, emath::easing, pos2, text::{LayoutJob, TextFormat}, vec2,
};
use iroh::EndpointId;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rc_ident::conn_code::ConnCode;
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        IsWindowVisible, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    },
};

use super::backend::{Ask, Backend, Dev, SessionInfo, SessionState, Snap, Status, Tone, Undoable};
use crate::{
    i18n::{self, LangPref, is_en, tr},
    meet,
    net::PathKind,
    theme,
};

const fn hex(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// White at `a`/255 over whatever is below, so a button or field looks
/// right on every surface (side pane, rows, toasts).
const fn veil(a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(a, a, a, a)
}

/// The content area on the right.
const BG: Color32 = hex(theme::LAYER);
/// The side pane on the left; the title bar has the same colour.
const PANE: Color32 = hex(theme::CHROME);
const CARD: Color32 = hex(theme::CARD);
const RAISED: Color32 = hex(theme::RAISED);
const BORDER: Color32 = hex(theme::BORDER);
const TEXT: Color32 = hex(theme::TEXT);
const DIM: Color32 = hex(theme::DIM);
/// Hints in empty fields.
const FAINT: Color32 = hex(0x6b6f77);
const GREEN: Color32 = hex(0x3fb950);
const AMBER: Color32 = hex(0xd29922);
/// Amber text, lighter than the amber of dots and bars.
const AMBER_TEXT: Color32 = hex(0xe3b34a);
const RED: Color32 = hex(0xe5534b);
/// Text of buttons that end or delete something.
const DANGER: Color32 = hex(0xff8a83);
const ACCENT: Color32 = hex(theme::ACCENT);
/// Text in the accent's colour, readable on the dark surfaces.
const LINK: Color32 = hex(0x8db6f0);
/// The dot of a device or relay that is not there.
const GONE: Color32 = hex(0x5a5e66);
/// Corners of buttons and text fields.
const CONTROL_RADIUS: u8 = 6;
/// Corners of the cards that hold rows and settings.
const GROUP_RADIUS: u8 = 10;
/// Around the text in text fields: as tall as a button.
const FIELD_MARGIN: Margin = Margin::symmetric(10, 6);
/// How long a toast stays, and one with 撤回.
const TOAST_LIFE: f32 = 4.5;
const TOAST_UNDO_LIFE: f32 = 8.0;
const TOAST_IN: f32 = 0.28;
const TOAST_OUT: f32 = 0.22;
/// The toasts at most on screen; a newer one pushes out the oldest.
const TOASTS_MAX: usize = 3;
/// Width of a popup menu.
const MENU_WIDTH: f32 = 236.0;
/// A device row, and where its text starts (past the picture).
const ROW_HEIGHT: f32 = 62.0;
const ROW_TEXT: f32 = 62.0;

/// The font families besides egui's own.
const BOLD: &str = "bold";
const ICONS: &str = "icons";

/// Segoe Fluent Icons (Windows 11), or Segoe MDL2 Assets (Windows 10) at the same code points.
mod ic {
    pub const DESKTOP: char = '\u{E7F4}';
    pub const COPY: char = '\u{E8C8}';
    pub const LINK: char = '\u{E71B}';
    pub const DEVICES: char = '\u{E772}';
    pub const GEAR: char = '\u{E713}';
    pub const MORE: char = '\u{E712}';
    pub const DEL: char = '\u{E74D}';
    pub const RENAME: char = '\u{E8AC}';
    pub const TERM: char = '\u{E756}';
    pub const CHECK: char = '\u{E73E}';
    pub const ADD: char = '\u{E8FA}';
    pub const WARN: char = '\u{E7BA}';
    pub const INFO: char = '\u{E946}';
    pub const OK: char = '\u{E930}';
    pub const SHIELD: char = '\u{EA18}';
    pub const CLOSE: char = '\u{E8BB}';
    pub const PLUS: char = '\u{E710}';
    pub const GLOBE: char = '\u{E774}';
    pub const WIFI: char = '\u{E701}';
    pub const SERVER: char = '\u{E968}';
    pub const LOCK: char = '\u{E72E}';
    pub const POWER: char = '\u{E7E8}';
    pub const BOT: char = '\u{E99A}';
    pub const SERVICE: char = '\u{E912}';
    pub const BOLT: char = '\u{E945}';
    pub const ERR: char = '\u{EA39}';
    pub const REFRESH: char = '\u{E72C}';
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Devices,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTarget {
    SelfName,
    Relays,
}

struct Toast {
    id: u64,
    tone: Tone,
    text: String,
    /// 撤回 takes back the pending deletion with this id.
    undo: Option<u64>,
    life: f32,
    /// Seconds shown without the pointer on it.
    shown: f32,
    born: Instant,
    closing: Option<Instant>,
    /// 撤回 was used (the deletion is not carried out then).
    acted: bool,
}

/// What the banners showed last, so they can fold away with their text.
#[derive(Default)]
struct Cards {
    trouble: String,
    busy: String,
    hosting: String,
    asking: String,
}

/// A device to delete once the user says so.
#[derive(Clone)]
struct Confirm {
    id: EndpointId,
    name: String,
    /// The other side can be told now.
    there: bool,
}

const RELAY_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const RELAY_PROBE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    Probing,
    Ok(Duration),
    Fail,
}

#[derive(Default)]
struct ProbeShared {
    wave: u64,
    in_flight: bool,
    results: HashMap<String, ProbeState>,
}

#[derive(Default)]
struct RelayProbe {
    shared: Arc<Mutex<ProbeShared>>,
    last_started: Option<Instant>,
    last_urls: Vec<String>,
    active: bool,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
}

impl RelayProbe {
    fn stop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(true);
        }
        if !self.active && self.last_started.is_none() && self.last_urls.is_empty() {
            return;
        }
        self.active = false;
        self.last_started = None;
        self.last_urls.clear();
        let mut sh = self.shared.lock().unwrap();
        sh.wave += 1;
        sh.in_flight = false;
        sh.results.clear();
    }

    fn snapshot(&self) -> (bool, HashMap<String, ProbeState>) {
        let sh = self.shared.lock().unwrap();
        (sh.in_flight, sh.results.clone())
    }

    fn start_wave(&mut self, ctx: &egui::Context, urls: &[String]) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(true);
        }
        let wave_id = {
            let mut sh = self.shared.lock().unwrap();
            sh.wave += 1;
            sh.in_flight = true;
            sh.results.retain(|k, _| urls.iter().any(|u| u == k));
            for u in urls {
                sh.results.entry(u.clone()).or_insert(ProbeState::Probing);
            }
            sh.wave
        };
        self.active = true;
        self.last_started = Some(Instant::now());
        self.last_urls = urls.to_vec();

        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        self.cancel = Some(cancel_tx);

        let shared = self.shared.clone();
        let ctx = ctx.clone();
        let targets = urls.to_vec();
        let spawned = std::thread::Builder::new().name("relay-probe".into()).spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                let mut sh = shared.lock().unwrap();
                if sh.wave == wave_id {
                    sh.in_flight = false;
                    for u in targets {
                        sh.results.insert(u, ProbeState::Fail);
                    }
                    ctx.request_repaint();
                }
                return;
            };
            rt.block_on(async move {
                let ca = rc_relay::tls::ca();
                let mut set = tokio::task::JoinSet::new();
                for raw_url in targets {
                    let ca = ca.clone();
                    set.spawn(async move {
                        let parsed: Vec<iroh::RelayUrl> = raw_url
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        let res = rc_relay::client::ping_rtt(&parsed, &ca, RELAY_PROBE_TIMEOUT).await;
                        (raw_url, res)
                    });
                }
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => {
                            set.abort_all();
                            break;
                        }
                        res = set.join_next() => {
                            let Some(res) = res else { break };
                            if let Ok((raw_url, ping_res)) = res {
                                let state = match ping_res {
                                    Ok(d) => ProbeState::Ok(d),
                                    Err(_) => ProbeState::Fail,
                                };
                                let mut sh = shared.lock().unwrap();
                                if sh.wave != wave_id {
                                    break;
                                }
                                sh.results.insert(raw_url, state);
                                ctx.request_repaint();
                            }
                        }
                    }
                }
                let mut sh = shared.lock().unwrap();
                if sh.wave == wave_id {
                    sh.in_flight = false;
                    ctx.request_repaint();
                }
            });
        });
        if spawned.is_err() {
            let mut sh = self.shared.lock().unwrap();
            if sh.wave == wave_id {
                sh.in_flight = false;
                for u in urls {
                    sh.results.insert(u.clone(), ProbeState::Fail);
                }
            }
        }
    }
}

#[derive(Clone, Default)]
enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(crate::update::Manifest),
    Downloading {
        version: String,
        done: u64,
        total: Option<u64>,
    },
    ReadyToApply(std::path::PathBuf),
    Error(String),
}

#[derive(Clone, Default)]
struct SelfUpdate {
    state: Arc<Mutex<UpdatePhase>>,
}

impl SelfUpdate {
    fn snapshot(&self) -> UpdatePhase {
        self.state.lock().unwrap().clone()
    }

    fn take_ready(&self) -> Option<std::path::PathBuf> {
        let mut s = self.state.lock().unwrap();
        if let UpdatePhase::ReadyToApply(path) = &*s {
            let out = path.clone();
            *s = UpdatePhase::Idle;
            Some(out)
        } else {
            None
        }
    }

    fn fail(&self, msg: String) {
        *self.state.lock().unwrap() = UpdatePhase::Error(msg);
    }

    fn start_check(&self, ctx: &egui::Context) {
        {
            let mut s = self.state.lock().unwrap();
            if matches!(*s, UpdatePhase::Checking | UpdatePhase::Downloading { .. } | UpdatePhase::ReadyToApply(_)) {
                return;
            }
            *s = UpdatePhase::Checking;
        }
        ctx.request_repaint();
        let state = self.state.clone();
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            let next = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => match rt.block_on(crate::update::check()) {
                    Ok(crate::update::CheckResult::UpToDate { .. }) => UpdatePhase::UpToDate,
                    Ok(crate::update::CheckResult::Available(m)) => UpdatePhase::Available(m),
                    Err(e) => UpdatePhase::Error(format!("{e:#}")),
                },
                Err(e) => UpdatePhase::Error(format!("{e:#}")),
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    fn start_download(&self, ctx: &egui::Context, manifest: crate::update::Manifest) {
        let version = manifest.version.trim().trim_start_matches('v').to_string();
        {
            let mut s = self.state.lock().unwrap();
            if matches!(*s, UpdatePhase::Downloading { .. } | UpdatePhase::ReadyToApply(_)) {
                return;
            }
            *s = UpdatePhase::Downloading { version: version.clone(), done: 0, total: None };
        }
        ctx.request_repaint();
        let state = self.state.clone();
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("update-dl".into()).spawn(move || {
            let mut last_repaint = Instant::now();
            let next = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => match rt.block_on(crate::update::download_and_stage(&manifest, |done, total| {
                    *state.lock().unwrap() = UpdatePhase::Downloading {
                        version: version.clone(),
                        done,
                        total,
                    };
                    if last_repaint.elapsed() >= Duration::from_millis(80) || Some(done) == total {
                        last_repaint = Instant::now();
                        ctx.request_repaint();
                    }
                })) {
                    Ok(path) => UpdatePhase::ReadyToApply(path),
                    Err(e) => UpdatePhase::Error(format!("{e:#}")),
                },
                Err(e) => UpdatePhase::Error(format!("{e:#}")),
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }
}

pub struct App {
    b: Backend,
    hwnd: Option<isize>,
    start_hidden_step: u8,
    page: Page,
    paste: String,
    copied: Option<Instant>,
    toasts: Vec<Toast>,
    next_id: u64,
    last_frame: Instant,
    /// The node's notice already shown.
    seen_notice: Option<String>,
    cards: Cards,
    /// 「以后」 on the banner saying so, until the window closes.
    update_later: bool,
    renaming: Option<(EndpointId, String)>,
    /// The unreachable device whose explanation is open.
    why: Option<EndpointId>,
    confirm: Option<Confirm>,
    /// The name in the settings while it is edited.
    name: String,
    saved: Option<Instant>,
    adding_relay: bool,
    relay_paste: String,
    relay_probe: RelayProbe,
    page_anim: Option<Instant>,
    scroll_to_dev: Option<EndpointId>,
    highlight_dev: Option<(EndpointId, Instant)>,
    scroll_to_settings: Option<SettingsTarget>,
    highlight_settings: Option<(SettingsTarget, Instant)>,
    focus_name: bool,
    skill_state: crate::term::SkillState,
    self_update: SelfUpdate,
}

/// `hidden`: started at sign-in, with only the icon in the tray.
pub fn run(b: Backend, hidden: bool) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Remote Control")
            .with_inner_size([920.0, 600.0])
            .with_min_inner_size([720.0, 460.0])
            // An empty icon: eframe would otherwise put its own (or one picture
            // rescaled for every size) on the window on the first frame, over
            // the exe's drawn sizes that `theme::set_icon` gives it.
            .with_icon(egui::IconData::default())
            .with_visible(!hidden),
        ..Default::default()
    };
    eframe::run_native(
        "remote-control",
        options,
        Box::new(move |cc| {
            setup(&cc.egui_ctx);
            // eframe keeps the window hidden until the first frame is drawn,
            // so the frame gets its colours before anyone sees it.
            let mut raw_hwnd = None;
            if let Ok(h) = cc.window_handle()
                && let RawWindowHandle::Win32(w) = h.as_raw()
            {
                raw_hwnd = Some(w.hwnd.get());
                let hwnd = HWND(w.hwnd.get() as *mut _);
                theme::frame(hwnd, theme::CHROME);
                theme::set_icon(hwnd);
                super::tray::set_window(hwnd);
                if hidden {
                    super::tray::start_hidden(hwnd);
                }
            }
            let ctx = cc.egui_ctx.clone();
            b.on_change(move || ctx.request_repaint());
            Ok(Box::new(App {
                b,
                hwnd: raw_hwnd,
                start_hidden_step: if hidden { 1 } else { 0 },
                page: Page::Devices,
                paste: String::new(),
                copied: None,
                toasts: Vec::new(),
                next_id: 1,
                last_frame: Instant::now(),
                seen_notice: None,
                cards: Cards::default(),
                renaming: None,
                why: None,
                confirm: None,
                name: String::new(),
                saved: None,
                adding_relay: false,
                relay_paste: String::new(),
                update_later: false,
                relay_probe: RelayProbe::default(),
                page_anim: None,
                scroll_to_dev: None,
                highlight_dev: None,
                scroll_to_settings: None,
                highlight_settings: None,
                focus_name: false,
                skill_state: crate::term::skill_state(),
                self_update: SelfUpdate::default(),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("lobby window: {e}"))
}

/// egui draws its own text and has no CJK glyphs: Microsoft YaHei from the
/// system goes first (its bold face for `BOLD`), egui's fonts stay as the
/// fallback; the system's icon font for `ICONS`.
fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let read = |paths: &[&str]| paths.iter().find_map(|p| std::fs::read(p).ok());
    if let Some(bytes) = read(&[r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\msyh.ttf", r"C:\Windows\Fonts\simsun.ttc"]) {
        fonts.font_data.insert("cjk".into(), Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "cjk".into());
        fonts.families.entry(FontFamily::Monospace).or_default().push("cjk".into());
    }
    let regular = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut bold = Vec::new();
    if let Some(bytes) = read(&[r"C:\Windows\Fonts\msyhbd.ttc", r"C:\Windows\Fonts\msyhbd.ttf"]) {
        fonts.font_data.insert("cjk-bold".into(), Arc::new(FontData::from_owned(bytes)));
        bold.push("cjk-bold".to_string());
    }
    bold.extend(regular.iter().cloned());
    fonts.families.insert(FontFamily::Name(BOLD.into()), bold);
    let mut icons = Vec::new();
    if let Some(bytes) = read(&[r"C:\Windows\Fonts\SegoeIcons.ttf", r"C:\Windows\Fonts\segmdl2.ttf"]) {
        fonts.font_data.insert("icons".into(), Arc::new(FontData::from_owned(bytes)));
        icons.push("icons".to_string());
    }
    icons.extend(regular);
    fonts.families.insert(FontFamily::Name(ICONS.into()), icons);
    ctx.set_fonts(fonts);
    // Dark whatever the system uses, and winit keeps the frame's caption
    // buttons dark too when the system theme changes.
    ctx.set_theme(egui::Theme::Dark);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANE;
    v.window_fill = RAISED;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(115) };
    v.popup_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(115) };
    v.extreme_bg_color = hex(theme::CHROME);
    v.text_edit_bg_color = Some(veil(10));
    v.faint_bg_color = veil(6);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.5);
    // Also the outline of the text field that has the focus.
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.override_text_color = Some(TEXT);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANE;
    w.noninteractive.weak_bg_fill = PANE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, veil(18));
    w.noninteractive.corner_radius = CornerRadius::same(CONTROL_RADIUS);
    for (state, fill, stroke) in
        [(&mut w.inactive, 16, 16), (&mut w.hovered, 26, 30), (&mut w.active, 10, 18), (&mut w.open, 26, 30)]
    {
        state.weak_bg_fill = veil(fill);
        state.bg_fill = veil(fill + 10);
        state.bg_stroke = Stroke::new(1.0, veil(stroke));
        state.fg_stroke = Stroke::new(1.5, TEXT);
        state.corner_radius = CornerRadius::same(CONTROL_RADIUS);
        state.expansion = 0.0;
    }
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.visuals = v;
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 6.0);
        s.spacing.menu_margin = Margin::same(4);
        // Win11-style floating scrollbar: thin at rest, widens smoothly on hover/drag.
        s.spacing.scroll = egui::style::ScrollStyle {
            floating: true,
            bar_width: 8.0,
            floating_width: 3.0,
            floating_allocated_width: 0.0,
            bar_inner_margin: 2.0,
            bar_outer_margin: 3.0,
            handle_min_length: 28.0,
            ..egui::style::ScrollStyle::floating()
        };
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
    });
    // winit emits 1.0 LineDelta per wheel notch regardless of Windows' scroll-lines
    // setting, and egui's 40pt default is ~2.5x slower than native Windows scrolling.
    let mut lines: u32 = 3;
    // SAFETY: `lines` is a valid out-pointer for a `u32` as required by `SPI_GETWHEELSCROLLLINES`.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&raw mut lines).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        lines = 3;
    }
    let speed = if lines == u32::MAX { 400.0 } else { (lines as f32) * 36.0 };
    ctx.options_mut(|o| o.input_options.line_scroll_speed = speed);
}

fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Draws the icon `c`, centred on `at`.
fn glyph(p: &egui::Painter, at: Pos2, c: char, size: f32, color: Color32) {
    p.text(at, Align2::CENTER_CENTER, c, FontId::new(size, FontFamily::Name(ICONS.into())), color);
}

/// `text` on one line of at most `width`, cut with "…".
fn one_line(ui: &egui::Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// A turning ring, for something going on.
fn spin(ui: &egui::Ui, at: Pos2, r: f32, width: f32, color: Color32) {
    let p = ui.painter();
    p.circle_stroke(at, r, Stroke::new(width, veil(38)));
    let start = ui.input(|i| i.time) as f32 * std::f32::consts::TAU / 0.8;
    let points = (0..=12)
        .map(|k| {
            let a = start + k as f32 / 12.0 * std::f32::consts::FRAC_PI_2;
            at + r * vec2(a.cos(), a.sin())
        })
        .collect();
    p.add(egui::Shape::line(points, Stroke::new(width, color)));
    ui.ctx().request_repaint();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Normal,
    /// The main action, filled with the accent.
    Primary,
    /// The accent, softly: 控制.
    Soft,
    /// No fill until the pointer is on it.
    Ghost,
    /// Ends or deletes something.
    Danger,
    Link,
}

struct Btn<'a> {
    text: &'a str,
    icon: Option<char>,
    look: Look,
    small: bool,
    min_width: f32,
    enabled: bool,
    color: Option<Color32>,
    /// The accessible name, when the text alone would repeat on every row.
    name: Option<String>,
}

impl<'a> Btn<'a> {
    fn new(text: &'a str) -> Self {
        Btn { text, icon: None, look: Look::Normal, small: false, min_width: 0.0, enabled: true, color: None, name: None }
    }

    fn look(mut self, look: Look) -> Self {
        self.look = look;
        self
    }

    fn icon(mut self, c: char) -> Self {
        self.icon = Some(c);
        self
    }

    fn small(mut self) -> Self {
        self.small = true;
        self
    }

    fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }

    fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    fn color(mut self, c: Color32) -> Self {
        self.color = Some(c);
        self
    }

    fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let (h, size, pad, radius) = if self.small { (22.0, 12.0, 8.0, 11.0) } else { (32.0, 14.0, 12.0, CONTROL_RADIUS as f32) };
        let g = ui.painter().layout_no_wrap(self.text.to_string(), font(size), Color32::PLACEHOLDER);
        let icon_w = if self.icon.is_some() { size + 6.0 } else { 0.0 };
        let w = (g.size().x + icon_w + 2.0 * pad).max(self.min_width);
        let on = self.enabled;
        let (rect, r) = ui.allocate_exact_size(vec2(w, h), if on { Sense::click() } else { Sense::hover() });
        let name = self.name.unwrap_or_else(|| self.text.to_string());
        r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, on, &name));
        let hot = on && (r.hovered() || r.has_focus());
        let down = on && r.is_pointer_button_down_on();
        let ht = ui.ctx().animate_bool_with_time(r.id.with("hot"), hot, 0.12);
        let none = Color32::TRANSPARENT;
        let (fill, edge, text) = match self.look {
            Look::Normal if self.color == Some(GREEN) => (GREEN.gamma_multiply(0.11), GREEN.gamma_multiply(0.28), GREEN),
            Look::Normal => (
                if down { veil(10) } else { veil(16).lerp_to_gamma(veil(26), ht) },
                veil(13).lerp_to_gamma(veil(24), ht),
                TEXT,
            ),
            Look::Primary => (
                if down {
                    hex(theme::ACCENT_PRESSED)
                } else {
                    ACCENT.lerp_to_gamma(hex(theme::ACCENT_HOVER), ht)
                },
                none,
                Color32::WHITE,
            ),
            Look::Soft if !on => (veil(10), none, DIM),
            Look::Soft => (
                ACCENT.gamma_multiply(if down { 0.14 } else { 0.16 + 0.10 * ht }),
                none,
                LINK.lerp_to_gamma(hex(0xb5d0f7), ht),
            ),
            Look::Ghost => (veil(if down { 10 } else { 16 }).gamma_multiply(ht), none, DIM.lerp_to_gamma(TEXT, ht)),
            Look::Danger => (RED.gamma_multiply(if down { 0.14 } else { 0.16 + 0.10 * ht }), none, DANGER),
            Look::Link => (ACCENT.gamma_multiply(0.14 * ht), none, LINK),
        };
        let text = self.color.unwrap_or(text);
        let fade = if on || self.look == Look::Soft { 1.0 } else { 0.4 };
        let draw_rect = if down { rect.shrink(0.5) } else { rect };
        let p = ui.painter();
        p.rect(draw_rect, radius, fill.gamma_multiply(fade), Stroke::new(1.0, edge.gamma_multiply(fade)), StrokeKind::Inside);
        let mut x = draw_rect.center().x - (icon_w + g.size().x) / 2.0;
        if let Some(c) = self.icon {
            glyph(p, pos2(x + size / 2.0, draw_rect.center().y), c, size, text.gamma_multiply(fade));
            x += icon_w;
        }
        p.galley(pos2(x, draw_rect.center().y - g.size().y / 2.0), g, text.gamma_multiply(fade));
        if on { r.on_hover_cursor(egui::CursorIcon::PointingHand) } else { r }
    }
}

/// A square button with an icon: no fill until the pointer is on it. `red`:
/// it deletes something, and turns red under the pointer.
fn icon_button(ui: &mut egui::Ui, c: char, size: f32, icon: f32, name: &str, red: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, name));
    let hot = r.hovered() || r.has_focus();
    if hot {
        let fill = if red { RED.gamma_multiply(0.16) } else { veil(16) };
        ui.painter().rect_filled(rect, CONTROL_RADIUS, fill);
    }
    let color = match (hot, red) {
        (true, true) => DANGER,
        (true, false) => TEXT,
        _ => DIM,
    };
    glyph(ui.painter(), rect.center(), c, icon, color);
    r.on_hover_text(name)
}

/// An on/off switch, Windows 11 style: a ring with a grey knob, filled with
/// the accent and a white knob when on.
fn switch(ui: &mut egui::Ui, on: &mut bool, enabled: bool, name: &str) -> egui::Response {
    let (rect, mut r) = ui.allocate_exact_size(vec2(40.0, 20.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    r.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, *on, name));
    let t = ui.ctx().animate_bool_with_time_and_easing(r.id, *on, 0.18, easing::cubic_out);
    let fade = if enabled { 1.0 } else { 0.35 };
    let hot = enabled && r.hovered();
    let down = enabled && r.is_pointer_button_down_on();
    let ht = ui.ctx().animate_bool_with_time(r.id.with("hot"), hot, 0.12);
    let ring = veil(115).lerp_to_gamma(veil(179), ht).lerp_to_gamma(ACCENT, t);
    let fill = Color32::TRANSPARENT.lerp_to_gamma(ACCENT, t);
    ui.painter().rect(rect, 10.0, fill.gamma_multiply(fade), Stroke::new(1.0, ring.gamma_multiply(fade)), StrokeKind::Inside);
    let radius = 6.0 + 0.8 * ht;
    let stretch = if down { 3.0 } else { 0.0 };
    let x = egui::lerp(rect.left() + 10.0 + stretch * 0.5..=rect.right() - 11.0 - stretch * 0.5, t);
    let knob = hex(0xc9cbd0).lerp_to_gamma(Color32::WHITE, t).gamma_multiply(fade);
    let knob_rect = Rect::from_center_size(pos2(x, rect.center().y), vec2(radius * 2.0 + stretch, radius * 2.0));
    ui.painter().rect_filled(knob_rect, radius, knob);
    if enabled { r.on_hover_cursor(egui::CursorIcon::PointingHand) } else { r }
}

/// Shows `add` folding open while `open`, and folding away after: its
/// height (measured on the frame before) grows from nothing, and it fades in.
fn reveal(ui: &mut egui::Ui, id: Id, open: bool, gap: f32, add: impl FnOnce(&mut egui::Ui)) {
    let t = ui.ctx().animate_bool_with_time_and_easing(id, open, 0.22, easing::cubic_out);
    if t <= 0.0 {
        return;
    }
    let full: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    let w = ui.available_width();
    let top = ui.cursor().min;
    let mut child = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(top, vec2(w, f32::INFINITY))));
    let shown = if full > 0.0 { full * t } else { 0.0 };
    child.set_clip_rect(ui.clip_rect().intersect(Rect::from_min_size(top, vec2(w, shown))));
    child.multiply_opacity(t);
    add(&mut child);
    let h = child.min_rect().height();
    ui.data_mut(|d| d.insert_temp(id, h));
    if full == 0.0 {
        // First frame: measured only; it shows from the next one.
        ui.ctx().request_repaint();
    }
    ui.allocate_space(vec2(w, (h + gap) * t));
}

#[derive(Clone, Copy)]
enum Tint {
    Red,
    Blue,
    Amber,
}

/// What a banner or a settings line starts with.
#[derive(Clone, Copy)]
enum Lead {
    Icon(char),
    Dot(Color32),
    Spin,
}

/// A banner: what is going on now, with its buttons.
fn banner(ui: &mut egui::Ui, tint: Tint, lead: Lead, text: &str, buttons: impl FnOnce(&mut egui::Ui)) {
    let (base, fg) = match tint {
        Tint::Red => (RED, DANGER),
        Tint::Blue => (ACCENT, LINK),
        Tint::Amber => (AMBER, AMBER_TEXT),
    };
    Frame::new()
        .fill(base.gamma_multiply(if matches!(tint, Tint::Red) { 0.12 } else { 0.10 }))
        .stroke(Stroke::new(1.0, base.gamma_multiply(0.24)))
        .corner_radius(8)
        .inner_margin(Margin { left: 14, right: 9, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().interact_size.y = 32.0;
            let full = ui.available_rect_before_wrap();
            let mut acts = ui.new_child(
                UiBuilder::new()
                    .max_rect(Rect::from_min_size(full.min, vec2(full.width(), 32.0)))
                    .layout(Layout::right_to_left(Align::Center)),
            );
            acts.spacing_mut().item_spacing.x = 6.0;
            buttons(&mut acts);
            let btn_left = if acts.min_rect().width() > 0.0 { acts.min_rect().left() - 12.0 } else { full.right() };
            let icon_r = Rect::from_min_size(full.min, vec2(18.0, 32.0));
            match lead {
                Lead::Icon(c) => glyph(ui.painter(), icon_r.center(), c, 18.0, fg),
                Lead::Spin => spin(ui, icon_r.center(), 7.0, 2.0, LINK),
                Lead::Dot(c) => {
                    ui.painter().circle_filled(icon_r.center(), 4.0, c);
                }
            }
            let text_x = full.left() + 18.0 + 12.0;
            let label_w = (btn_left - text_x).max(40.0);
            let g_h = ui.painter().layout(text.to_string(), font(14.0), TEXT, label_w).size().y;
            let top_pad = ((32.0 - g_h) / 2.0).max(0.0);
            let mut left = ui.new_child(
                UiBuilder::new().max_rect(Rect::from_min_size(pos2(text_x, full.top() + top_pad), vec2(label_w, f32::INFINITY))),
            );
            left.add(egui::Label::new(RichText::new(text).size(14.0)).wrap());
            let total_h = (top_pad + left.min_rect().height()).max(acts.min_rect().height()).max(32.0);
            ui.allocate_exact_size(vec2(full.width(), total_h), Sense::hover());
        });
}

/// A section's title, with a note after it.
fn section_head(ui: &mut egui::Ui, title: &str, note: Option<&str>) {
    section_head_with(ui, title, note, |_| {});
}

/// A section's title, with a note after it and optional controls on the right.
fn section_head_with(ui: &mut egui::Ui, title: &str, note: Option<&str>, right: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(bold(13.0)));
        if let Some(n) = note {
            ui.label(RichText::new(n).size(12.0).color(DIM));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
}

/// The card that holds a section's rows or lines.
fn group(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    group_highlighted(ui, 0.0, add)
}

fn group_highlighted(ui: &mut egui::Ui, hi: f32, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let fill = if hi > 0.0 { CARD.lerp_to_gamma(ACCENT, 0.10 * hi) } else { CARD };
    let stroke = if hi > 0.0 { Stroke::new(1.0 + 0.5 * hi, veil(8).lerp_to_gamma(ACCENT, hi)) } else { Stroke::new(1.0, veil(8)) };
    Frame::new()
        .fill(fill)
        .corner_radius(GROUP_RADIUS)
        .stroke(stroke)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
        })
        .response
}

/// One line of a settings card: an icon (or a dot), a title, a line under
/// it, the controls on the right, all centred on the line.
struct Item<'a> {
    title: &'a str,
    title_font: FontId,
    desc: Option<(&'a str, Color32)>,
    desc_suffix: Option<(String, Color32)>,
    lead: Lead,
    /// The card's first line: no divider above.
    first: bool,
    enabled: bool,
    right_width: f32,
}

impl<'a> Item<'a> {
    fn new(lead: Lead, title: &'a str) -> Self {
        Item { title, title_font: font(14.0), desc: None, desc_suffix: None, lead, first: false, enabled: true, right_width: 260.0 }
    }

    fn desc(mut self, d: &'a str) -> Self {
        self.desc = Some((d, DIM));
        self
    }

    fn first(mut self, first: bool) -> Self {
        self.first = first;
        self
    }

    fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    fn right_width(mut self, w: f32) -> Self {
        self.right_width = w;
        self
    }

    fn show(self, ui: &mut egui::Ui, right: impl FnOnce(&mut egui::Ui)) {
        const PAD: Vec2 = Vec2::new(14.0, 10.0);
        let right_w = self.right_width;
        let w = ui.available_width();
        let indent = 34.0;
        let text_w = (w - 2.0 * PAD.x - right_w - indent).max(160.0);
        let fade = if self.enabled { 1.0 } else { 0.45 };
        let title = ui.painter().layout(self.title.to_string(), self.title_font, TEXT.gamma_multiply(fade), text_w);
        let desc = self.desc.map(|(d, c)| {
            let mut job = LayoutJob::default();
            job.wrap.max_width = text_w;
            job.append(d, 0.0, TextFormat::simple(font(12.0), c.gamma_multiply(fade)));
            if let Some((suf, sc)) = self.desc_suffix {
                job.append(&suf, 0.0, TextFormat::simple(font(12.0), sc.gamma_multiply(fade)));
            }
            ui.painter().layout_job(job)
        });
        let text_h = title.size().y + desc.as_ref().map_or(0.0, |g| g.size().y + 1.0);
        let h = (text_h + 2.0 * PAD.y).max(62.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        let p = ui.painter();
        if !self.first {
            p.hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(13)));
        }
        let x = rect.left() + PAD.x;
        let y = rect.center().y - text_h / 2.0;
        let lead_at = pos2(x + 10.0, rect.center().y);
        match self.lead {
            Lead::Icon(c) => glyph(p, lead_at, c, 18.0, DIM.gamma_multiply(fade)),
            Lead::Dot(c) => {
                p.circle_filled(lead_at, 3.5, c.gamma_multiply(fade));
            }
            Lead::Spin => spin(ui, lead_at, 6.0, 1.5, DIM),
        }
        p.galley(pos2(x + indent, y), title.clone(), TEXT);
        if let Some(g) = desc {
            p.galley(pos2(x + indent, y + title.size().y + 1.0), g, DIM);
        }
        let right_rect = Rect::from_min_max(pos2(rect.right() - PAD.x - right_w, rect.top()), pos2(rect.right() - PAD.x, rect.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(right_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if !self.enabled {
                ui.disable();
            }
            right(ui);
        });
        // The scope moves the cursor to below what it holds, which sits in
        // the middle of the line (or is nothing): the next line starts below this one.
        ui.advance_cursor_after_rect(rect);
    }
}

/// A wide button with an icon in the side pane; `on`: the page shown, with
/// a bar of the accent at its left. `count`: on the right.
fn nav(ui: &mut egui::Ui, c: char, text: &str, on: bool, count: Option<usize>) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));
    let on_t = ui.ctx().animate_bool_with_time_and_easing(r.id.with("on"), on, 0.20, easing::cubic_out);
    let hot_t = ui.ctx().animate_bool_with_time(r.id.with("hot"), r.hovered(), 0.12);
    let p = ui.painter();
    let alpha = (16.0 * on_t).max(10.0 * hot_t).round() as u8;
    if alpha > 0 {
        p.rect_filled(rect, CONTROL_RADIUS, veil(alpha));
    }
    if on_t > 0.02 {
        let half = 8.0 * on_t;
        let cy = rect.center().y;
        let bar = Rect::from_min_max(pos2(rect.left(), cy - half), pos2(rect.left() + 3.0, cy + half));
        p.rect_filled(bar, 2.0, ACCENT.gamma_multiply(on_t));
    }
    let ic_color = DIM.lerp_to_gamma(TEXT, on_t.max(hot_t * 0.7));
    glyph(p, pos2(rect.left() + 20.0, rect.center().y), c, 16.0, ic_color);
    p.text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, text, font(14.0), TEXT);
    if let Some(n) = count {
        p.text(pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, n.to_string(), font(12.0), DIM);
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One line of a popup menu: clickable across its width, lit while the
/// pointer is on it (red for `danger`). `sub`: dim text after the title;
/// `tick`: the choice in force.
fn menu_item_enabled(
    ui: &mut egui::Ui,
    icon: Option<char>,
    text: &str,
    sub: Option<&str>,
    tick: bool,
    danger: bool,
    enabled: bool,
) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), if enabled { Sense::click() } else { Sense::hover() });
    r.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled, tick, text));
    let t = if enabled { ui.ctx().animate_bool_with_time(r.id, ui.rect_contains_pointer(rect), 0.1) } else { 0.0 };
    let p = ui.painter();
    if t > 0.0 {
        let fill = if danger { RED.gamma_multiply(0.16 * t) } else { veil(16).gamma_multiply(t) };
        p.rect_filled(rect, CONTROL_RADIUS, fill);
    }
    let color = if !enabled { FAINT } else if danger { DANGER } else { TEXT };
    if let Some(c) = icon {
        let ic_color = if !enabled { FAINT } else if danger { DANGER } else { DIM };
        glyph(p, pos2(rect.left() + 18.0, rect.center().y), c, 14.0, ic_color);
    }
    let g = p.layout_no_wrap(text.to_string(), font(14.0), color);
    let x = rect.left() + 36.0;
    let w = g.size().x;
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, color);
    if let Some(s) = sub {
        p.text(pos2(x + w + 8.0, rect.center().y), Align2::LEFT_CENTER, s, font(12.0), if enabled { DIM } else { FAINT });
    }
    if tick {
        glyph(p, pos2(rect.right() - 16.0, rect.center().y), ic::CHECK, 12.0, if enabled { LINK } else { FAINT });
    }
    if enabled { r.on_hover_cursor(egui::CursorIcon::PointingHand) } else { r }
}

fn menu_item(ui: &mut egui::Ui, icon: Option<char>, text: &str, sub: Option<&str>, tick: bool, danger: bool) -> egui::Response {
    menu_item_enabled(ui, icon, text, sub, tick, danger, true)
}

fn menu_sep(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(rect.left() + 6.0..=rect.right() - 6.0, rect.center().y, Stroke::new(1.0, veil(18)));
}

fn menu_head(ui: &mut egui::Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    ui.painter().text(pos2(rect.left() + 10.0, rect.bottom() - 4.0), Align2::LEFT_BOTTOM, text, font(12.0), DIM);
}

fn ago(s: u64) -> String {
    if is_en() {
        match s {
            0..60 => "just now".into(),
            60..3600 => format!("{} min ago", s / 60),
            _ => format!("{} hr ago", s / 3600),
        }
    } else {
        match s {
            0..60 => "刚才".into(),
            60..3600 => format!("{} 分钟前", s / 60),
            _ => format!("{} 小时前", s / 3600),
        }
    }
}

/// The line under a device's name: its colour, and whether something is
/// going on (a spinner before it).
fn sub_line(d: &Dev, session: Option<SessionInfo>) -> (String, Color32, bool) {
    if d.ask == Ask::In {
        return (
            tr(
                "想记住你。同意后互相记住，以后随时能连",
                "Wants to remember you. Agree so both sides can connect anytime",
            )
            .into(),
            AMBER_TEXT,
            false,
        );
    }
    match session {
        Some(SessionInfo { state: SessionState::Connecting, .. }) => return (tr("正在连接…", "Connecting…").into(), DIM, true),
        Some(SessionInfo { state: SessionState::Waiting, since }) => {
            let left = 30u64.saturating_sub(since.elapsed().as_secs());
            let s = if is_en() {
                if left > 0 {
                    format!("Waiting for approval · {left}s")
                } else {
                    "Waiting for approval".into()
                }
            } else if left > 0 {
                format!("等对方允许 · {left} 秒")
            } else {
                "等对方允许".into()
            };
            return (s, DIM, true);
        }
        Some(SessionInfo { state: SessionState::Connected, .. }) => return (tr("控制中", "Controlling").into(), LINK, false),
        Some(SessionInfo { state: SessionState::Paused, .. }) => {
            return (tr("控制中 · 画面暂停", "Controlling · Video paused").into(), AMBER_TEXT, false);
        }
        None => {}
    }
    if d.ask == Ask::Out {
        return (tr("等对方同意", "Waiting for acceptance").into(), DIM, true);
    }
    let via = match d.met.as_str() {
        _ if d.listed => "",
        "code" => tr(" · 连接码", " · Code"),
        "lan" => tr(" · 局域网", " · LAN"),
        _ => "",
    };
    let (s, color, busy) = match d.status {
        Status::Online { rtt_ms, path } => {
            let way = match path {
                PathKind::Direct => tr(" · 直连", " · Direct"),
                PathKind::Relay => tr(" · 经自建中转", " · Private Relay"),
                PathKind::Public => tr(" · 正在找直连", " · Finding Direct"),
                PathKind::None => "",
            };
            let label = if is_en() {
                format!("Online · {:.0} ms{way}", rtt_ms.max(1.0))
            } else {
                format!("在线 · {:.0} ms{way}", rtt_ms.max(1.0))
            };
            (label, DIM, false)
        }
        Status::Checking => (tr("正在连接…", "Connecting…").into(), DIM, true),
        Status::Offline => (tr("离线", "Offline").into(), DIM, false),
        Status::Unreachable => (tr("无法连接", "Unable to connect").into(), AMBER_TEXT, false),
        Status::Refused(r) => (meet::refusal_text(r).into(), AMBER_TEXT, false),
    };
    (format!("{s}{via}"), color, busy)
}

fn tone_look(t: Tone) -> (char, Color32) {
    match t {
        Tone::Ok => (ic::OK, GREEN),
        Tone::Info => (ic::INFO, LINK),
        Tone::Warn => (ic::WARN, AMBER),
        Tone::Err => (ic::ERR, DANGER),
    }
}

/// `relay.example.com` of `https://relay.example.com:7843/`; an address stays whole.
fn short_host(urls: &str) -> String {
    let first = host_of(urls.split(',').next().unwrap_or(""));
    let h = first.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(first.as_str(), |(h, _)| h);
    let h = h.trim_matches(['[', ']']);
    if h.chars().any(|c| c.is_ascii_alphabetic()) && !h.contains(':') {
        h.split('.').next().unwrap_or(h).to_string()
    } else {
        h.to_string()
    }
}

impl eframe::App for App {
    /// What shows around the content area's round corner.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        PANE.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self) {
        self.relay_probe.stop();
        self.b.flush_undoables();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        match self.start_hidden_step {
            1 => {
                if super::tray::is_start_hidden() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    ctx.request_repaint();
                    self.start_hidden_step = 2;
                } else {
                    self.start_hidden_step = 0;
                }
            }
            2 => {
                self.start_hidden_step = 0;
                if super::tray::is_start_hidden() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                super::tray::finish_start_hidden();
            }
            _ => {}
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray();
        }
        let snap = self.b.snap();
        self.take_notices(&snap);
        if let Some(stage) = self.self_update.take_ready() {
            let active = !self.b.sessions().is_empty() || snap.hosting.is_some() || snap.asking.is_some();
            if active {
                let msg = tr("当前有远控连接，请先断开后再更新", "Disconnect active remote sessions before updating");
                self.self_update.fail(msg.into());
                self.toast(Tone::Warn, msg);
            } else {
                let svc = self.b.service_installed() && !self.b.service_other();
                self.b.prepare_for_self_update();
                if let Err(e) = crate::update::launch_apply_and_exit(&stage, svc, true) {
                    let msg = format!("{e:#}");
                    self.self_update.fail(msg.clone());
                    self.toast(Tone::Err, msg);
                }
            }
        }
        if self.confirm.is_none() {
            self.paste_anywhere(&ctx);
        }
        let esc = ctx.memory(|m| m.focused().is_none()) && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if esc && self.confirm.is_some() {
            self.confirm = None;
        } else if esc && self.page == Page::Settings {
            self.set_page(Page::Devices);
        }
        if self.page != Page::Devices {
            self.renaming = None;
        }
        egui::Panel::left("me")
            .resizable(false)
            .show_separator_line(false)
            .exact_size(236.0)
            .frame(Frame::new().fill(PANE).inner_margin(Margin { left: 12, right: 12, top: 8, bottom: 14 }))
            .show(ui, |ui| self.left(ui, &snap));
        // The content area lies on the pane like a sheet: round where it
        // meets the pane and the title bar, with a faint edge there. It runs
        // past the window's right and bottom edges, so the edge shows only
        // on the left and at the top.
        let layer = Frame::new()
            .fill(BG)
            .corner_radius(CornerRadius { nw: 8, ne: 0, sw: 0, se: 0 })
            .stroke(Stroke::new(1.0, veil(12)))
            .outer_margin(Margin { left: 0, right: -2, top: 0, bottom: -2 })
            .inner_margin(Margin { left: 27, right: 9, top: 1, bottom: 0 });
        let page_t = match self.page_anim {
            Some(t0) => {
                let p = (t0.elapsed().as_secs_f32() / 0.18).clamp(0.0, 1.0);
                if p >= 1.0 {
                    self.page_anim = None;
                    1.0
                } else {
                    ctx.request_repaint();
                    easing::cubic_out(p)
                }
            }
            None => 1.0,
        };
        let content = egui::CentralPanel::default()
            .frame(layer)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Room for the scroll bar on the right, and above and below the rows
                    // (inside the scrolled part, so rows scroll up to the edge).
                    ui.set_width(ui.available_width() - 18.0);
                    if page_t < 1.0 {
                        ui.multiply_opacity(0.45 + 0.55 * page_t);
                    }
                    ui.add_space(21.0 + 6.0 * (1.0 - page_t));
                    match self.page {
                        Page::Devices => self.devices(ui, &snap),
                        Page::Settings => self.settings(ui, &snap),
                    }
                    ui.add_space(16.0);
                });
            })
            .response
            .rect;
        if self.page != Page::Settings || !self.is_visible(&ctx) {
            self.relay_probe.stop();
        }
        self.toasts(&ctx, content);
        self.confirm_dialog(&ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl App {
    fn is_visible(&self, ctx: &egui::Context) -> bool {
        let shown = self.hwnd.is_none_or(|h| unsafe { IsWindowVisible(HWND(h as *mut _)).as_bool() });
        shown && !ctx.input(|i| i.viewport().minimized.unwrap_or(false))
    }

    /// Closing the window: it goes to the tray, and counts as closed.
    fn hide_to_tray(&mut self) {
        self.relay_probe.stop();
        self.b.flush_undoables();
        self.toasts.clear();
        self.renaming = None;
        self.confirm = None;
        self.update_later = false;
        super::tray::hide();
        self.b.set_visible(false);
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        self.push_toast(tone, text.into(), None);
    }

    fn push_toast(&mut self, tone: Tone, text: String, undo: Option<u64>) {
        let open = self.toasts.iter().filter(|t| t.closing.is_none()).count();
        if open >= TOASTS_MAX
            && let Some(old) = self.toasts.iter_mut().find(|t| t.closing.is_none())
        {
            old.closing = Some(Instant::now());
        }
        let id = self.new_id();
        let life = if undo.is_some() { TOAST_UNDO_LIFE } else { TOAST_LIFE };
        self.toasts.push(Toast { id, tone, text, undo, life, shown: 0.0, born: Instant::now(), closing: None, acted: false });
    }

    /// Hides `what` at once and carries it out when its toast is gone,
    /// unless 撤回 was clicked.
    fn undoable(&mut self, what: Undoable, text: String) {
        let id = self.new_id();
        self.b.defer_undoable(id, what);
        self.push_toast(Tone::Info, text, Some(id));
    }

    /// Deleting a device takes effect at once, on both sides: waiting for an
    /// undo would leave the other side showing it for as long.
    fn delete(&mut self, c: Confirm) {
        if self.renaming.as_ref().is_some_and(|(id, _)| *id == c.id) {
            self.renaming = None;
        }
        self.b.act(&format!("remove\t{}", c.id));
        let msg = if is_en() {
            let there = if c.there {
                "also removed from the remote device"
            } else {
                "it will also remove this device when back online"
            };
            format!("Removed \"{}\"; {there}", c.name)
        } else {
            let there = if c.there { "对方那边也删掉了本机" } else { "对方上线时也会删掉本机" };
            format!("已删除「{}」，{there}", c.name)
        };
        self.toast(Tone::Ok, msg);
    }

    fn hidden_relay(&self, url: &str) -> bool {
        self.b.has_pending_relay(url)
    }

    fn set_page(&mut self, page: Page) {
        if self.page != page {
            if page != Page::Devices {
                self.renaming = None;
            }
            if page == Page::Settings {
                self.skill_state = crate::term::skill_state();
            }
            self.page = page;
            self.page_anim = Some(Instant::now());
        }
    }

    fn jump_settings(&mut self, target: SettingsTarget) {
        self.why = None;
        self.set_page(Page::Settings);
        self.scroll_to_settings = Some(target);
        self.highlight_settings = Some((target, Instant::now()));
        if target == SettingsTarget::SelfName {
            self.focus_name = true;
        }
    }

    fn settings_highlight(&mut self, ctx: &egui::Context, target: SettingsTarget) -> f32 {
        let Some((t, at)) = self.highlight_settings else { return 0.0 };
        if t != target {
            return 0.0;
        }
        let p = at.elapsed().as_secs_f32() / 1.1;
        if p >= 1.0 {
            self.highlight_settings = None;
            0.0
        } else {
            ctx.request_repaint();
            1.0 - easing::cubic_out(p)
        }
    }

    fn dev_highlight(&mut self, ctx: &egui::Context, id: EndpointId) -> f32 {
        let Some((hid, at)) = self.highlight_dev else { return 0.0 };
        if hid != id {
            return 0.0;
        }
        let p = at.elapsed().as_secs_f32() / 1.1;
        if p >= 1.0 {
            self.highlight_dev = None;
            0.0
        } else {
            ctx.request_repaint();
            1.0 - easing::cubic_out(p)
        }
    }

    /// Messages of the node and of the backend become toasts.
    fn take_notices(&mut self, snap: &Snap) {
        if let Some((n, text)) = &snap.notice
            && self.seen_notice.as_ref() != Some(n)
        {
            self.seen_notice = Some(n.clone());
            self.b.act(&format!("dismiss\t{n}"));
            let remembered = text.contains("互相记住") || text.contains("remembered each other");
            let tone = if remembered { Tone::Ok } else { Tone::Warn };
            let msg = if remembered && !text.contains("我的电脑") && !text.contains("My Devices") {
                if is_en() {
                    format!("{text}; if this is your own PC, set it as \"My Devices\" via ···")
                } else {
                    format!("{text}；若为自己的电脑，可在 ··· 设为「我的电脑」")
                }
            } else {
                text.clone()
            };
            self.toast(tone, msg);
        }
        for n in self.b.take_notices() {
            self.toast(n.tone, n.text);
        }
    }

    /// Ctrl+V with no text field focused: a connection code goes in.
    fn paste_anywhere(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let pasted = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(t) => Some(t.clone()),
                _ => None,
            })
        });
        if let Some(t) = pasted {
            self.submit_code(&t);
        }
    }

    fn submit_code(&mut self, text: &str) {
        let line = match ConnCode::find(text) {
            Some((c, _)) => c.encode(),
            None => text.split_whitespace().collect::<Vec<_>>().join(" "),
        };
        match self.b.request(&format!("paste\t{line}")) {
            Ok(id) => {
                self.paste.clear();
                self.set_page(Page::Devices);
                let trimmed = id.trim();
                if let Ok(eid) = trimmed.parse::<EndpointId>() {
                    self.scroll_to_dev = Some(eid);
                    self.highlight_dev = Some((eid, Instant::now()));
                }
                // The card may have been there already (found on the LAN): say it went in.
                let snap = self.b.snap();
                let dev = snap.devs.iter().find(|d| d.id.to_string() == trimmed);
                let text = match dev {
                    Some(d) if matches!(d.status, Status::Online { .. }) => {
                        if is_en() {
                            format!("Added \"{}\". Click \"Control\" to start or \"Remember\" to save", d.name)
                        } else {
                            format!("已添加「{}」，点「控制」开始或点「记住」保存", d.name)
                        }
                    }
                    Some(d) => {
                        if is_en() {
                            format!("Added \"{}\", finding connection…", d.name)
                        } else {
                            format!("已添加「{}」，正在查找连接…", d.name)
                        }
                    }
                    None => tr("已提交连接码，正在查找对方…", "Connection code submitted, looking up device…").to_string(),
                };
                self.toast(Tone::Ok, text);
            }
            Err(e) => self.toast(Tone::Err, format!("{e:#}")),
        }
    }

    fn left(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        ui.spacing_mut().item_spacing.y = 2.0;
        // This computer (clickable to jump to device name in Settings).
        let (rect, me_resp) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::click());
        me_resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, tr("修改本机设备名称", "Change this device's name")));
        let me_ht = ui.ctx().animate_bool_with_time(me_resp.id.with("hot"), me_resp.hovered(), 0.14);
        let p = ui.painter();
        if me_ht > 0.0 {
            p.rect_filled(rect, 8.0, veil((12.0 * me_ht).round() as u8));
        }
        let av = Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - 20.0), Vec2::splat(40.0));
        p.rect_filled(av, 10.0, ACCENT.gamma_multiply(0.16));
        glyph(p, av.center(), ic::DESKTOP, 20.0, LINK);
        let x = av.right() + 12.0;
        let name = if snap.name.is_empty() { "…" } else { &snap.name };
        let g = one_line(ui, name, bold(16.0), TEXT, rect.right() - x - 8.0);
        let top = av.center().y - (g.size().y + 17.0) / 2.0;
        let gh = g.size().y;
        ui.painter().galley(pos2(x, top), g, TEXT);
        let (dot, state) = if snap.me.is_empty() {
            (GONE, tr("本机 · 正在启动", "This PC · Starting"))
        } else {
            (GREEN, tr("本机 · 在线", "This PC · Online"))
        };
        let y = top + gh + 9.0;
        ui.painter().circle_filled(pos2(x + 3.5, y), 3.5, dot);
        ui.painter().text(pos2(x + 13.0, y), Align2::LEFT_CENTER, state, font(12.0), DIM);
        if me_resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tr("点击前往设置修改设备名称", "Click to change device name in Settings"))
            .clicked()
        {
            self.jump_settings(SettingsTarget::SelfName);
        }
        ui.add_space(10.0);

        if nav(ui, ic::DEVICES, tr("设备", "Devices"), self.page == Page::Devices, Some(snap.devs.len())).clicked() {
            self.set_page(Page::Devices);
        }
        if nav(ui, ic::GEAR, tr("设置", "Settings"), self.page == Page::Settings, None).clicked() {
            self.set_page(Page::Settings);
        }

        let relays: Vec<_> = snap.relays.iter().filter(|r| !self.hidden_relay(&r.url)).collect();
        if relays.is_empty() {
            return;
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            let h = 16.0 + 18.0 + 4.0 + (relays.len() as f32) * 22.0;
            let (card, r) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
            r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, tr("管理自建中转", "Manage private relays")));
            let ht = ui.ctx().animate_bool_with_time(r.id.with("hot"), r.hovered(), 0.14);
            let p = ui.painter();
            if ht > 0.0 {
                p.rect_filled(card, 8.0, veil((12.0 * ht).round() as u8));
            }
            let head_y = card.top() + 8.0 + 9.0;
            p.text(pos2(card.left() + 8.0, head_y), Align2::LEFT_CENTER, tr("自建中转", "Private Relays"), font(12.0), DIM);
            if ht > 0.02 {
                p.text(
                    pos2(card.right() - 8.0, head_y),
                    Align2::RIGHT_CENTER,
                    tr("管理", "Manage"),
                    font(11.0),
                    LINK.gamma_multiply(ht),
                );
            }
            let mut ry = card.top() + 8.0 + 18.0 + 4.0;
            for relay in &relays {
                let row_rect = Rect::from_min_size(pos2(card.left(), ry), vec2(card.width(), 22.0));
                let (color, state) = match relay.state.as_str() {
                    "home" => (GREEN, tr("已连", "Connected")),
                    "temp" => (GREEN, tr("临时接管", "Fallback")),
                    "fail" => (AMBER, tr("连不上", "Unreachable")),
                    "idle" => (GONE, tr("备用", "Standby")),
                    _ => (GONE, "…"),
                };
                let p = ui.painter();
                p.circle_filled(pos2(row_rect.left() + 11.5, row_rect.center().y), 3.5, color);
                let right = p.layout_no_wrap(state.to_string(), font(12.0), DIM);
                let rw = right.size().x;
                p.galley(pos2(row_rect.right() - 8.0 - rw, row_rect.center().y - right.size().y / 2.0), right, DIM);
                let host = one_line(ui, &short_host(&relay.url), font(12.0), TEXT, row_rect.width() - 23.0 - rw - 16.0);
                let hy = row_rect.center().y - host.size().y / 2.0;
                ui.painter().galley(pos2(row_rect.left() + 23.0, hy), host, TEXT);
                ry += 22.0;
            }
            if r.on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(tr("前往设置管理自建中转", "Manage private relays in Settings"))
                .clicked()
            {
                self.jump_settings(SettingsTarget::Relays);
            }
        });
    }

    /// What is going on now, above the device list; each banner folds away
    /// when it no longer holds.
    fn banners(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        const GAP: f32 = 10.0;
        let trouble = self.b.trouble();
        if let Some(t) = &trouble {
            self.cards.trouble = t.clone();
        }
        let text = self.cards.trouble.clone();
        reveal(ui, Id::new("card-trouble"), trouble.is_some(), GAP, |ui| banner(ui, Tint::Blue, Lead::Spin, &text, |_| {}));

        let busy = self.b.busy();
        if let Some(t) = &busy {
            self.cards.busy = t.clone();
        }
        let text = self.cards.busy.clone();
        reveal(ui, Id::new("card-busy"), busy.is_some(), GAP, |ui| banner(ui, Tint::Blue, Lead::Spin, &text, |_| {}));

        if let Some(n) = &snap.hosting {
            self.cards.hosting = if is_en() {
                format!("\"{n}\" is controlling this PC")
            } else {
                format!("「{n}」正在控制本机")
            };
        }
        let text = self.cards.hosting.clone();
        let b = self.b.clone();
        reveal(ui, Id::new("card-hosting"), snap.hosting.is_some(), GAP, |ui| {
            banner(ui, Tint::Red, Lead::Icon(ic::DESKTOP), &text, |ui| {
                if Btn::new(tr("断开", "Disconnect")).look(Look::Danger).show(ui).clicked() {
                    b.act("disconnect");
                }
            })
        });

        if let Some(n) = &snap.asking {
            self.cards.asking = if is_en() {
                format!("\"{n}\" wants to control this PC — allow or decline in the prompt")
            } else {
                format!("「{n}」请求控制本机，在弹窗里允许或拒绝")
            };
        }
        let text = self.cards.asking.clone();
        reveal(ui, Id::new("card-asking"), snap.asking.is_some(), GAP, |ui| {
            banner(ui, Tint::Amber, Lead::Icon(ic::SHIELD), &text, |_| {})
        });

        let suggest = snap.suggest.is_some() && snap.hosting.is_none();
        let idle = self.b.busy().is_none();
        reveal(ui, Id::new("card-suggest"), suggest, GAP, |ui| {
            banner(
                ui,
                Tint::Blue,
                Lead::Icon(ic::SERVICE),
                tr("装上后台服务，锁屏时也能被控制", "Install the background service to allow control on the lock screen"),
                |ui| {
                    if Btn::new(tr("安装", "Install")).icon(ic::SHIELD).look(Look::Primary).enabled(idle).show(ui).clicked() {
                        b.act("unsuggest");
                        b.service(true);
                    }
                    if Btn::new(tr("以后", "Later")).look(Look::Ghost).show(ui).clicked() {
                        b.act("unsuggest");
                    }
                },
            )
        });

        let other = self.b.service_other() && !self.update_later;
        let mut later = false;
        reveal(ui, Id::new("card-update"), other, GAP, |ui| {
            banner(
                ui,
                Tint::Blue,
                Lead::Icon(ic::SERVICE),
                tr(
                    "后台服务用的是另一个位置的程序，改用这个",
                    "The background service points to a different executable — switch to this one",
                ),
                |ui| {
                    if Btn::new(tr("改用这个", "Use This Version"))
                        .icon(ic::SHIELD)
                        .look(Look::Primary)
                        .enabled(idle)
                        .show(ui)
                        .clicked()
                    {
                        b.service(true);
                    }
                    later = Btn::new(tr("以后", "Later")).look(Look::Ghost).show(ui).clicked();
                },
            )
        });
        self.update_later |= later;
    }

    fn devices(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.label(RichText::new(tr("设备", "Devices")).font(bold(22.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let done = self.copied.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                let tip = tr(
                    "一次性有效（1 小时），对方连入时会弹窗等你允许",
                    "One-time use (valid for 1 hour); you will be asked to approve incoming connections",
                );
                if done {
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                    Btn::new(tr("已复制，1 小时内有效", "Copied · valid for 1 hr"))
                        .icon(ic::CHECK)
                        .color(GREEN)
                        .show(ui)
                        .on_hover_text(tip);
                } else if Btn::new(tr("复制本机连接码", "Copy Connection Code"))
                    .icon(ic::COPY)
                    .enabled(!snap.me.is_empty())
                    .show(ui)
                    .on_hover_text(tip)
                    .clicked()
                {
                    match self.b.request("code") {
                        Ok(code) => {
                            ui.ctx().copy_text(code);
                            self.copied = Some(Instant::now());
                        }
                        Err(e) => self.toast(Tone::Err, format!("{e:#}")),
                    }
                }
            });
        });
        ui.add_space(6.0);
        self.connect_field(ui);
        let codes = &snap.codes;
        if !codes.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                ui.spacing_mut().interact_size.y = 26.0;
                ui.label(
                    RichText::new(tr(
                        "还没用掉的连接码（连入时会先问你）",
                        "Unused connection codes (you'll be asked before connecting)",
                    ))
                    .size(12.0)
                    .color(DIM),
                );
                for c in codes {
                    Frame::new().fill(veil(13)).corner_radius(13).inner_margin(Margin { left: 10, right: 2, top: 2, bottom: 2 }).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.spacing_mut().interact_size.y = 22.0;
                        ui.horizontal(|ui| {
                            let age = ago(c.age_s);
                            let label = if is_en() { format!("Copied {age}") } else { format!("{age}复制") };
                            let acc = if is_en() {
                                format!("Revoke connection code copied {age}")
                            } else {
                                format!("撤销 {age}复制的连接码")
                            };
                            ui.label(RichText::new(label).size(12.0));
                            let r = Btn::new(tr("撤销", "Revoke")).look(Look::Ghost).small().name(acc).show(ui);
                            if r.clicked() {
                                self.b.act(&format!("revoke\t{}", c.token));
                                self.toast(Tone::Info, tr("已撤销，这个连接码不能再用了", "Revoked — this connection code can no longer be used"));
                            }
                        });
                    });
                }
            });
        }
        ui.add_space(14.0);
        self.banners(ui, snap);

        let sessions = self.b.sessions();
        let at_own_relay = snap.relays.iter().any(|r| r.state == "home");
        type Kind = (&'static str, &'static str, fn(&Dev) -> bool);
        let kinds: [Kind; 3] = [
            (tr("我的电脑", "My Devices"), tr("连入本机免确认", "Auto-accept connections"), |d| d.listed && d.auto),
            (tr("信任设备", "Trusted Devices"), tr("连入本机需确认", "Ask before connecting"), |d| d.listed && !d.auto),
            (tr("临时设备", "Temporary Devices"), tr("关窗口后移除", "Removed when window closes"), |d| !d.listed),
        ];
        let mut any = false;
        for (title, note, is) in kinds {
            let ds: Vec<&Dev> = snap.devs.iter().filter(|d| is(d)).collect();
            if ds.is_empty() {
                continue;
            }
            any = true;
            section_head(ui, title, Some(note));
            let last = ds.len() - 1;
            group(ui, |ui| {
                for (i, d) in ds.into_iter().enumerate() {
                    self.row(ui, d, sessions.get(&d.id).map(|(_, info)| *info), i == 0, i == last, at_own_relay);
                }
            });
            ui.add_space(12.0);
        }
        if !any && !snap.me.is_empty() {
            ui.add_space(56.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(ic::DEVICES.to_string()).font(FontId::new(40.0, FontFamily::Name(ICONS.into()))).color(FAINT));
                ui.add_space(4.0);
                ui.label(tr("还没有设备", "No devices yet"));
                ui.label(
                    RichText::new(tr(
                        "同一局域网会自动出现；跨网请复制连接码发给对方，或贴到上方",
                        "Devices on the same LAN appear automatically; across networks, copy your code or paste one above",
                    ))
                    .size(12.0)
                    .color(DIM),
                );
            });
        }
    }

    /// Where the other side's connection code goes: a code goes in as soon
    /// as it is pasted (or typed whole).
    fn connect_field(&mut self, ui: &mut egui::Ui) {
        let id = Id::new("connect");
        let focused = ui.memory(|m| m.has_focus(id));
        let focus_t = ui.ctx().animate_bool_with_time(id.with("focus"), focused, 0.14);
        let mut submit = None;
        let border = veil(16).lerp_to_gamma(ACCENT, focus_t);
        let bg = veil(10).lerp_to_gamma(veil(14), focus_t);
        let frame = Frame::new()
            .fill(bg)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(8)
            .inner_margin(Margin { left: 14, right: 6, top: 0, bottom: 0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().interact_size.y = 40.0;
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                    glyph(ui.painter(), r.center(), ic::LINK, 16.0, DIM.lerp_to_gamma(LINK, focus_t));
                    let empty = self.paste.trim().is_empty();
                    let btn_w = if empty { 0.0 } else { 58.0 };
                    let mut edit = TextEdit::singleline(&mut self.paste)
                        .id(id)
                        .frame(Frame::NONE)
                        .font(font(14.0))
                        .margin(Margin::symmetric(0, 4))
                        .desired_width((ui.available_width() - btn_w).max(60.0));
                    if !focused {
                        edit = edit.hint_text(
                            RichText::new(tr("粘贴对方连接码，加入设备列表", "Paste a connection code to add a device")).color(FAINT),
                        );
                    }
                    let e = ui.add(edit);
                    if e.changed() && ConnCode::find(&self.paste).is_some() {
                        submit = Some(self.paste.clone());
                    }
                    if e.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.paste.trim().is_empty() {
                        submit = Some(self.paste.clone());
                    }
                    if !empty && Btn::new(tr("添加", "Add")).look(Look::Soft).small().show(ui).clicked() {
                        submit = Some(self.paste.clone());
                    }
                });
            });
        if focus_t > 0.01 {
            ui.painter().rect_stroke(
                frame.response.rect.expand(1.5),
                9.5,
                Stroke::new(2.5, ACCENT.gamma_multiply(0.18 * focus_t)),
                StrokeKind::Outside,
            );
        }
        if submit.is_none() && frame.response.interact(Sense::click()).clicked() {
            ui.memory_mut(|m| m.request_focus(id));
        }
        if let Some(t) = submit {
            self.submit_code(&t);
        }
    }

    /// One device: its picture (what it is to this computer, a dot for its
    /// status), its name and what is going on, its buttons. Double-click:
    /// control it (rename lives in the `···` menu).
    /// `at_own_relay`: this computer is at its own relay, so a device it
    /// cannot get through to is not at that relay.
    fn row(&mut self, ui: &mut egui::Ui, d: &Dev, session: Option<SessionInfo>, first: bool, last: bool, at_own_relay: bool) {
        if session.is_some_and(|s| s.state == SessionState::Waiting) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        let online = matches!(d.status, Status::Online { .. });
        let unreachable = d.status == Status::Unreachable;
        let asked = d.ask == Ask::In;
        let why_open = unreachable && self.why == Some(d.id);
        let (rect, row) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
        if self.scroll_to_dev == Some(d.id) {
            self.scroll_to_dev = None;
            ui.scroll_to_rect(rect, Some(Align::Center));
        }
        let hi = self.dev_highlight(ui.ctx(), d.id);
        let row_ht = ui.ctx().animate_bool_with_time(row.id.with("hot"), row.hovered(), 0.12);
        let p = ui.painter().clone();
        let top = if first { GROUP_RADIUS } else { 0 };
        let bottom = if last && !why_open { GROUP_RADIUS } else { 0 };
        let corners = CornerRadius { nw: top, ne: top, sw: bottom, se: bottom };
        if asked {
            p.rect_filled(rect, corners, AMBER.gamma_multiply(0.07));
            let bar = Rect::from_min_max(pos2(rect.left(), rect.top() + 10.0), pos2(rect.left() + 3.0, rect.bottom() - 10.0));
            p.rect_filled(bar, 2.0, AMBER);
        } else if hi > 0.0 {
            p.rect_filled(rect, corners, ACCENT.gamma_multiply(0.22 * hi));
        } else if row_ht > 0.0 {
            p.rect_filled(rect, corners, veil((8.0 * row_ht).round() as u8));
        }
        if !first {
            p.hline(rect.left() + ROW_TEXT..=rect.right(), rect.top(), Stroke::new(1.0, veil(13)));
        }
        // The picture.
        let av = Rect::from_min_size(pos2(rect.left() + 14.0, rect.center().y - 18.0), Vec2::splat(36.0));
        let (fill, fg) = if !d.listed {
            (veil(16), DIM)
        } else if d.auto {
            (ACCENT.gamma_multiply(0.16), LINK)
        } else {
            (hex(0x8b7cf6).gamma_multiply(0.16), hex(0xb6a8ff))
        };
        p.rect_filled(av, 9.0, fill);
        glyph(&p, av.center(), ic::DESKTOP, 18.0, fg);
        let dot = av.right_bottom() - vec2(3.0, 3.0);
        p.circle_filled(dot, 6.0, CARD);
        let dot_color = if session.is_some() { ACCENT } else if online { GREEN } else if unreachable { AMBER } else { GONE };
        p.circle_filled(dot, 4.0, dot_color);

        // The buttons, from the right.
        let acts_rect = Rect::from_min_max(pos2(rect.left() + ROW_TEXT, rect.top()), pos2(rect.right() - 10.0, rect.bottom()));
        let mut acts = ui.new_child(UiBuilder::new().max_rect(acts_rect).layout(Layout::right_to_left(Align::Center)));
        acts.spacing_mut().item_spacing.x = 6.0;
        self.row_menu(&mut acts, d);
        if asked {
            let agree_name = if is_en() { format!("Agree {}", d.name) } else { format!("同意 {}", d.name) };
            if Btn::new(tr("同意", "Agree")).look(Look::Primary).name(agree_name).show(&mut acts).clicked() {
                match self.b.request(&format!("agree\t{}", d.id)) {
                    Ok(_) => {
                        self.scroll_to_dev = Some(d.id);
                        self.highlight_dev = Some((d.id, Instant::now()));
                        let msg = if is_en() {
                            format!("Remembered \"{}\" mutually; if this is your own PC, set it as \"My Devices\" via ···", d.name)
                        } else {
                            format!("已和「{}」互相记住；若为自己的电脑，可在 ··· 设为「我的电脑」", d.name)
                        };
                        self.toast(Tone::Ok, msg);
                    }
                    Err(e) => self.toast(Tone::Err, format!("{e:#}")),
                }
            }
            let decline_name = if is_en() { format!("Decline {}", d.name) } else { format!("拒绝 {}", d.name) };
            if Btn::new(tr("拒绝", "Decline")).look(Look::Ghost).name(decline_name).show(&mut acts).clicked() {
                self.b.act(&format!("refuse\t{}", d.id));
            }
        } else {
            match session {
                Some(SessionInfo { state: SessionState::Connecting | SessionState::Waiting, .. }) => {
                    let acc = if is_en() { format!("Cancel controlling {}", d.name) } else { format!("取消控制 {}", d.name) };
                    if Btn::new(tr("取消", "Cancel")).look(Look::Ghost).name(acc).show(&mut acts).clicked() {
                        self.b.cancel_control(d.id);
                    }
                }
                Some(_) => {}
                None if unreachable => {
                    let acc = if is_en() { format!("Details for {}", d.name) } else { format!("查看原因 {}", d.name) };
                    if Btn::new(tr("查看原因", "Details")).look(Look::Ghost).name(acc).show(&mut acts).clicked() {
                        self.why = if self.why == Some(d.id) { None } else { Some(d.id) };
                    }
                }
                None => {
                    let acc = if is_en() { format!("Control {}", d.name) } else { format!("控制 {}", d.name) };
                    let r = Btn::new(tr("控制", "Control")).look(Look::Soft).enabled(online).name(acc).show(&mut acts);
                    if r.clicked() {
                        self.b.control(d.id, &d.name);
                    }
                }
            }
            if !d.listed && d.ask == Ask::None {
                let acc = if is_en() { format!("Remember {}", d.name) } else { format!("记住 {}", d.name) };
                let r = Btn::new(tr("记住", "Remember")).look(Look::Ghost).icon(ic::ADD).name(acc).show(&mut acts);
                if r.clicked() {
                    match self.b.request(&format!("remember\t{}", d.id)) {
                        Ok(both) if both.trim() == "both" => {
                            self.scroll_to_dev = Some(d.id);
                            self.highlight_dev = Some((d.id, Instant::now()));
                            let msg = if is_en() {
                                format!("Remembered \"{}\" mutually; if this is your own PC, set it as \"My Devices\" via ···", d.name)
                            } else {
                                format!("已和「{}」互相记住；若为自己的电脑，可在 ··· 设为「我的电脑」", d.name)
                            };
                            self.toast(Tone::Ok, msg);
                        }
                        Ok(_) => {}
                        Err(e) => self.toast(Tone::Err, format!("{e:#}")),
                    }
                }
            } else if !d.listed && d.ask == Ask::Out && session.is_none() {
                let acc = if is_en() { format!("Cancel remember {}", d.name) } else { format!("取消记住 {}", d.name) };
                if Btn::new(tr("取消", "Cancel")).look(Look::Ghost).name(acc).show(&mut acts).clicked() {
                    self.b.act(&format!("cancel-remember\t{}", d.id));
                }
            }
        }
        let acts_used = acts.min_rect();
        let on_acts = ui.input(|i| i.pointer.latest_pos().is_some_and(|pos| acts_used.contains(pos)));
        if online && session.is_none() && !asked && self.renaming.is_none() && !on_acts && row.double_clicked() {
            self.b.control(d.id, &d.name);
        }
        let text_right = acts_used.left() - 12.0;

        // The name and the line under it.
        let x = rect.left() + ROW_TEXT;
        let w = text_right - x;
        let (sub, sub_color, busy) = sub_line(d, session);
        let name = one_line(ui, &d.name, font(15.0), TEXT, w);
        let sub = one_line(ui, &sub, font(12.0), sub_color, w - if busy { 17.0 } else { 0.0 });
        let (nh, sh) = (name.size().y, sub.size().y);
        let y = rect.center().y - (nh + 1.0 + sh) / 2.0;
        let renaming = self.renaming.as_ref().is_some_and(|(id, _)| *id == d.id);
        if renaming {
            let at = Rect::from_min_size(pos2(x - 8.0, y - 5.0), vec2(220.0_f32.min(w + 8.0), nh + 8.0));
            let mut field = ui.new_child(UiBuilder::new().max_rect(at));
            let (_, text) = self.renaming.as_mut().unwrap();
            let e = field.add(TextEdit::singleline(text).font(font(15.0)).desired_width(at.width() - 16.0).margin(Margin::symmetric(8, 2)));
            if !e.has_focus() && !e.lost_focus() {
                e.request_focus();
            }
            if field.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.renaming = None;
            } else if e.lost_focus() {
                let (id, text) = self.renaming.take().unwrap();
                let cleaned = text.replace(['\t', '\r', '\n'], " ");
                let trimmed = cleaned.trim();
                if !trimmed.is_empty() && trimmed != d.name {
                    self.b.act(&format!("rename\t{id}\t{trimmed}"));
                }
            }
        } else {
            p.galley(pos2(x, y), name, TEXT);
        }
        let mut sx = x;
        if busy {
            spin(ui, pos2(x + 5.5, y + nh + 1.0 + sh / 2.0), 5.0, 1.5, DIM);
            sx += 17.0;
        }
        p.galley(pos2(sx, y + nh + 1.0), sub, sub_color);

        let b = self.b.clone();
        let mut to_settings = false;
        reveal(ui, Id::new(("why", d.id)), why_open, 0.0, |ui| {
            Frame::new().inner_margin(Margin { left: ROW_TEXT as i8, right: 14, top: 0, bottom: 14 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 8.0;
                let why = if at_own_relay {
                    tr(
                        "对方连不上你的自建中转，也无法建立直连。可能是它那边的网络连不到中转服务器。",
                        "The remote device cannot reach your private relay or establish a direct connection. Its network may be blocking access to the relay.",
                    )
                } else {
                    tr(
                        "两台电脑之间无法直连，通常是网络限制。可开启路由器 IPv6，或在设置中添加自建中转。",
                        "Cannot establish a direct connection, usually due to NAT/firewall restrictions. Try enabling IPv6 on your router or adding a private relay in Settings.",
                    )
                };
                ui.label(RichText::new(why).size(13.0).color(DIM));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if Btn::new(tr("重试", "Retry")).show(ui).clicked() {
                        b.act(&format!("retry\t{}", d.id));
                    }
                    to_settings = Btn::new(tr("去设置", "Go to Settings")).show(ui).clicked();
                });
            });
        });
        if to_settings {
            self.jump_settings(SettingsTarget::Relays);
        }
    }

    fn row_menu(&mut self, ui: &mut egui::Ui, d: &Dev) {
        let online = matches!(d.status, Status::Online { .. });
        let more_label = if is_en() { format!("More for {}", d.name) } else { format!("更多 {}", d.name) };
        let more = icon_button(ui, ic::MORE, 32.0, 16.0, &more_label, false);
        egui::Popup::menu(&more).align(RectAlign::BOTTOM_END).gap(4.0).show(|ui| {
            ui.set_width(MENU_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;
            if menu_item_enabled(ui, Some(ic::TERM), tr("终端", "Terminal"), None, false, false, online).clicked() {
                ui.close();
                if let Err(e) = crate::term::open_window(d.id, &d.name) {
                    let msg = if is_en() { format!("Could not open terminal: {e:#}") } else { format!("打不开终端：{e:#}") };
                    self.toast(Tone::Err, msg);
                }
            }
            if d.listed {
                if menu_item(ui, Some(ic::RENAME), tr("重命名", "Rename"), None, false, false).clicked() {
                    self.renaming = Some((d.id, d.name.clone()));
                    ui.close();
                }
                menu_sep(ui);
                menu_head(ui, tr("它控制本机时", "When controlling this PC"));
                for (auto, title, sub) in [
                    (true, tr("我的电脑", "My Devices"), tr("免确认", "Auto-accept")),
                    (false, tr("信任设备", "Trusted Devices"), tr("需确认", "Ask first")),
                ] {
                    if menu_item(ui, None, title, Some(sub), d.auto == auto, false).clicked() {
                        ui.close();
                        if d.auto != auto {
                            self.b.act(&format!("auto\t{}\t{}", d.id, auto as u8));
                            self.scroll_to_dev = Some(d.id);
                            self.highlight_dev = Some((d.id, Instant::now()));
                            let msg = if auto {
                                if is_en() {
                                    format!(
                                        "\"{}\" can now connect without confirmation (set it on the other PC too for two-way auto-accept)",
                                        d.name
                                    )
                                } else {
                                    format!("「{}」以后连入本机免确认（如需双向免确认，请在对方电脑同样设置）", d.name)
                                }
                            } else if is_en() {
                                format!("\"{}\" will now ask before connecting to this PC", d.name)
                            } else {
                                format!("「{}」以后连入本机需确认", d.name)
                            };
                            self.toast(Tone::Ok, msg);
                        }
                    }
                }
            }
            menu_sep(ui);
            if menu_item(ui, Some(ic::DEL), tr("删除", "Remove"), None, false, true).clicked() {
                ui.close();
                let there = matches!(d.status, Status::Online { .. } | Status::Unreachable);
                self.confirm = Some(Confirm { id: d.id, name: d.name.clone(), there });
            }
        });
    }

    /// 删除 asks first: the window dims under a small dialog.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = self.confirm.clone() else { return };
        let screen = ctx.content_rect();
        egui::Area::new(Id::new("veil")).order(Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let (r, _) = ui.allocate_exact_size(screen.size(), Sense::click());
            ui.painter().rect_filled(r, 0.0, Color32::from_black_alpha(128));
        });
        let (mut cancel, mut delete) = (false, false);
        egui::Area::new(Id::new("confirm")).order(Order::Tooltip).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            Frame::new()
                .fill(RAISED)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(12)
                .inner_margin(20)
                .shadow(Shadow { offset: [0, 24], blur: 60, spread: 0, color: Color32::from_black_alpha(140) })
                .show(ui, |ui| {
                    ui.set_width(340.0);
                    ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.x = 14.0;
                        let (badge, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                        ui.painter().rect_filled(badge, 10.0, RED.gamma_multiply(0.16));
                        glyph(ui.painter(), badge.center(), ic::DEL, 20.0, DANGER);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            let title = if is_en() { format!("Remove \"{}\"?", c.name) } else { format!("删除「{}」？", c.name) };
                            ui.add(egui::Label::new(RichText::new(title).font(bold(16.0))).wrap());
                            ui.add(
                                egui::Label::new(
                                    RichText::new(tr(
                                        "两台电脑都会忘掉对方，本机中转马上停止为它转发。以后要再连，重新发连接码。",
                                        "Both PCs will forget each other, and this PC's relay will stop forwarding for it immediately. To reconnect later, share a new connection code.",
                                    ))
                                    .size(13.0)
                                    .color(DIM),
                                )
                                .wrap(),
                            );
                        });
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.add_space(ui.available_width() - 2.0 * 84.0 - 8.0);
                        cancel = Btn::new(tr("取消", "Cancel")).min_width(84.0).show(ui).clicked();
                        delete = Btn::new(tr("删除", "Remove")).look(Look::Danger).min_width(84.0).show(ui).clicked();
                    });
                });
        });
        if delete {
            self.confirm = None;
            self.delete(c);
        } else if cancel {
            self.confirm = None;
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.label(RichText::new(tr("设置", "Settings")).font(bold(22.0)));
        });
        ui.add_space(6.0);
        let b = self.b.clone();

        section_head(ui, tr("本机", "This Device"), None);
        let self_hi = self.settings_highlight(ui.ctx(), SettingsTarget::SelfName);
        let focus_name = std::mem::take(&mut self.focus_name);
        let self_resp = group_highlighted(ui, self_hi, |ui| {
            let name = &mut self.name;
            let saved = &mut self.saved;
            Item::new(Lead::Icon(ic::RENAME), tr("设备名称", "Device Name"))
                .desc(tr("其他设备看到的本机名称", "Name shown to other devices"))
                .first(true)
                .right_width(276.0)
                .show(ui, |ui| {
                    let e = ui.add(TextEdit::singleline(name).hint_text(&snap.name).desired_width(170.0).margin(FIELD_MARGIN));
                    if !e.has_focus() && !e.lost_focus() {
                        name.clone_from(&snap.set_name);
                    }
                    if focus_name {
                        e.request_focus();
                    }
                    if e.lost_focus() {
                        let cleaned = name.replace(['\t', '\r', '\n'], " ");
                        let trimmed = cleaned.trim().to_string();
                        if trimmed != *name {
                            *name = trimmed.clone();
                        }
                        if trimmed != snap.set_name {
                            b.act(&format!("set-name\t{trimmed}"));
                            *saved = Some(Instant::now());
                        }
                    }
                    if let Some(t) = *saved {
                        let a = 1.0 - ((t.elapsed().as_secs_f32() - 1.4) / 0.3).clamp(0.0, 1.0);
                        if a > 0.0 {
                            ui.ctx().request_repaint();
                            ui.label(RichText::new(tr("✓ 已保存", "✓ Saved")).size(12.0).color(GREEN.gamma_multiply(a)));
                        }
                    }
                });
            let cur_lang = i18n::pref();
            Item::new(Lead::Icon(ic::GLOBE), tr("语言", "Language"))
                .desc(tr(
                    "界面显示语言（跟随系统默认按 Windows 语言切换）",
                    "Interface language (Auto follows Windows UI language)",
                ))
                .right_width(276.0)
                .show(ui, |ui| {
                    Frame::new()
                        .fill(veil(8))
                        .stroke(Stroke::new(1.0, veil(16)))
                        .corner_radius(CONTROL_RADIUS + 1)
                        .inner_margin(Margin::same(3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                for (p, label) in [
                                    (LangPref::Auto, tr("跟随系统", "Auto")),
                                    (LangPref::Zh, "简体中文"),
                                    (LangPref::En, "English"),
                                ] {
                                    let look = if cur_lang == p { Look::Soft } else { Look::Ghost };
                                    if Btn::new(label).look(look).show(ui).clicked() && cur_lang != p {
                                        b.set_language(p);
                                    }
                                }
                            });
                        });
                });
        });
        if self.scroll_to_settings == Some(SettingsTarget::SelfName) {
            self.scroll_to_settings = None;
            ui.scroll_to_rect(self_resp.rect, Some(Align::TOP));
        }

        ui.add_space(12.0);
        section_head(ui, tr("网络与发现", "Network & Discovery"), None);
        group(ui, |ui| {
            let mut public = snap.public;
            let public_title = tr("公网辅助连接", "Public Relay Assist");
            Item::new(Lead::Icon(ic::GLOBE), public_title)
                .desc(tr(
                    "跨网直连时用于交换地址，不转发画面和数据",
                    "Exchanges addresses for cross-network P2P; never forwards video or data",
                ))
                .first(true)
                .show(ui, |ui| {
                    if switch(ui, &mut public, true, public_title).changed() {
                        b.act(&format!("set\tpublic\t{}", public as u8));
                    }
                });
            let mut lan = snap.lan;
            let lan_title = tr("局域网发现", "LAN Discovery");
            Item::new(Lead::Icon(ic::WIFI), lan_title)
                .desc(tr("允许同一局域网内的设备自动发现本机", "Allow devices on the same LAN to discover this PC"))
                .show(ui, |ui| {
                    if switch(ui, &mut lan, true, lan_title).changed() {
                        b.act(&format!("set\tlan\t{}", lan as u8));
                    }
                });
        });

        ui.add_space(12.0);
        self.relays(ui, snap);

        ui.add_space(12.0);
        let other = self.b.service_other();
        section_head(ui, tr("后台服务", "Background Service"), None);
        group(ui, |ui| {
            let installed = b.service_installed();
            let busy = b.busy();
            let (title, desc) = match (installed, snap.service) {
                (true, true) if other => (
                    tr("运行中（程序路径不一致）", "Running (different executable path)"),
                    tr("点击「改用这个」以使用当前版本", "Click \"Use This Version\" to switch to this executable"),
                ),
                (true, true) => (
                    tr("运行中", "Running"),
                    tr("关闭窗口后仍保持在线", "Keeps this PC online after closing the window"),
                ),
                (true, false) => (
                    tr("已安装（未运行）", "Installed (not running)"),
                    tr("安装后支持开机在线与锁屏控制（需要管理员权限）", "Enables online at boot & lock-screen control (requires admin)"),
                ),
                _ => (
                    tr("未安装", "Not installed"),
                    tr("安装后支持开机在线与锁屏控制（需要管理员权限）", "Enables online at boot & lock-screen control (requires admin)"),
                ),
            };
            Item::new(Lead::Icon(ic::SERVICE), title).desc(desc).first(true).right_width(280.0).show(ui, |ui| {
                if let Some(what) = &busy {
                    let label = if what.contains("卸载") || what.to_ascii_lowercase().contains("uninstall") {
                        tr("卸载中…", "Uninstalling…")
                    } else {
                        tr("安装中…", "Installing…")
                    };
                    Btn::new(label).enabled(false).show(ui);
                } else if !installed {
                    if Btn::new(tr("安装", "Install")).icon(ic::SHIELD).look(Look::Primary).show(ui).clicked() {
                        b.service(true);
                    }
                } else {
                    if installed && other && Btn::new(tr("改用这个", "Use This Version")).icon(ic::SHIELD).look(Look::Primary).show(ui).clicked() {
                        b.service(true);
                    }
                    if Btn::new(tr("卸载", "Uninstall")).icon(ic::SHIELD).show(ui).clicked() {
                        b.service(false);
                    }
                }
            });
            let on = snap.service;
            for (key, icon, title, desc, value) in [
                (
                    "boot",
                    ic::POWER,
                    tr("开机自启", "Start with Windows"),
                    tr("系统启动后自动在后台保持在线", "Stay online in the background after Windows starts"),
                    snap.boot,
                ),
                (
                    "lock",
                    ic::LOCK,
                    tr("允许锁屏控制", "Allow Lock Screen Control"),
                    tr("仅限「我的电脑」免确认连入", "Auto-accept connections from \"My Devices\" only"),
                    snap.lock,
                ),
                (
                    "awake",
                    ic::BOLT,
                    tr("接通电源时阻止睡眠", "Keep Awake on AC Power"),
                    tr("保持在线，显示器仍按系统设置熄屏", "Stay online while allowing the display to turn off"),
                    snap.awake,
                ),
            ] {
                let mut v = if on { value } else { false };
                let shown_desc = if on { desc } else { tr("安装后台服务后可用", "Available after installing the background service") };
                Item::new(Lead::Icon(icon), title)
                    .desc(shown_desc)
                    .enabled(on)
                    .show(ui, |ui| {
                        if switch(ui, &mut v, on, title).changed() {
                            b.act(&format!("set\t{key}\t{}", v as u8));
                        }
                    });
            }
        });

        ui.add_space(12.0);
        enum SkillAct {
            Install,
            Update,
            Uninstall,
        }
        let mut skill_act = None;
        let skill_desc = match self.skill_state {
            crate::term::SkillState::NotInstalled => tr(
                "为 Claude Code、Codex 安装远程控制 Skill，支持通过终端操作设备",
                "Install the remote-control Skill for Claude Code & Codex",
            ),
            crate::term::SkillState::Installed => tr("已安装 · 支持 Claude Code、Codex", "Installed · Claude Code & Codex"),
            crate::term::SkillState::Outdated => tr("已安装（当前程序路径或版本有变化）", "Installed (executable path or version changed)"),
        };
        section_head(ui, tr("AI 集成", "AI Integration"), None);
        group(ui, |ui| {
            Item::new(Lead::Icon(ic::BOT), tr("配置 AI Skills", "Configure AI Skills"))
                .desc(skill_desc)
                .first(true)
                .show(ui, |ui| match self.skill_state {
                    crate::term::SkillState::NotInstalled => {
                        if Btn::new(tr("安装", "Install")).show(ui).clicked() {
                            skill_act = Some(SkillAct::Install);
                        }
                    }
                    crate::term::SkillState::Installed => {
                        if Btn::new(tr("卸载", "Uninstall")).show(ui).clicked() {
                            skill_act = Some(SkillAct::Uninstall);
                        }
                    }
                    crate::term::SkillState::Outdated => {
                        if Btn::new(tr("卸载", "Uninstall")).show(ui).clicked() {
                            skill_act = Some(SkillAct::Uninstall);
                        }
                        if Btn::new(tr("更新", "Update")).look(Look::Primary).show(ui).clicked() {
                            skill_act = Some(SkillAct::Update);
                        }
                    }
                });
        });
        if let Some(act) = skill_act {
            match act {
                SkillAct::Install => match crate::term::install_skill() {
                    Ok(where_) => {
                        let msg = if is_en() {
                            format!("Installed to {where_}; takes effect in new AI chats")
                        } else {
                            format!("已写入 {where_}，新开的 AI 对话即生效")
                        };
                        self.toast(Tone::Ok, msg);
                    }
                    Err(e) => {
                        let msg = if is_en() { format!("Install failed: {e:#}") } else { format!("安装失败：{e:#}") };
                        self.toast(Tone::Err, msg);
                    }
                },
                SkillAct::Update => match crate::term::install_skill() {
                    Ok(_) => self.toast(Tone::Ok, tr("已更新 AI Skills", "Updated AI Skills")),
                    Err(e) => {
                        let msg = if is_en() { format!("Update failed: {e:#}") } else { format!("更新失败：{e:#}") };
                        self.toast(Tone::Err, msg);
                    }
                },
                SkillAct::Uninstall => match crate::term::uninstall_skill() {
                    Ok(()) => self.toast(Tone::Ok, tr("已卸载 AI Skills", "Uninstalled AI Skills")),
                    Err(e) => {
                        let msg = if is_en() { format!("Uninstall failed: {e:#}") } else { format!("卸载失败：{e:#}") };
                        self.toast(Tone::Err, msg);
                    }
                },
            }
            self.skill_state = crate::term::skill_state();
        }

        ui.add_space(12.0);
        let phase = self.self_update.snapshot();
        let active_session = !self.b.sessions().is_empty() || snap.hosting.is_some() || snap.asking.is_some();
        let title = format!("Remote Control v{}", crate::update::APP_VERSION);
        let desc = match &phase {
            UpdatePhase::Idle => tr("手动检查新版本，平时不会主动打扰", "Check for updates manually; never prompts in background").to_string(),
            UpdatePhase::Checking => tr("正在检查更新…", "Checking for updates…").to_string(),
            UpdatePhase::UpToDate => tr("已是最新版本", "You are on the latest version").to_string(),
            UpdatePhase::Available(m) => {
                let next = m.version.trim().trim_start_matches('v');
                let note = m.notes.lines().next().map(str::trim).unwrap_or("");
                if is_en() {
                    if note.is_empty() {
                        format!("New version v{next} available")
                    } else {
                        format!("New version v{next} available · {note}")
                    }
                } else if note.is_empty() {
                    format!("发现新版本 v{next}")
                } else {
                    format!("发现新版本 v{next} · {note}")
                }
            }
            UpdatePhase::Downloading { version, done, total } => match total.filter(|&t| t > 0) {
                Some(t) => {
                    let pct = done.saturating_mul(100) / t;
                    if is_en() {
                        format!("Downloading v{version} · {pct}% ({:.1} / {:.1} MB)", *done as f64 / 1e6, t as f64 / 1e6)
                    } else {
                        format!("正在下载 v{version} · {pct}%（{:.1} / {:.1} MB）", *done as f64 / 1e6, t as f64 / 1e6)
                    }
                }
                None => {
                    if is_en() {
                        format!("Downloading v{version} ({:.1} MB)…", *done as f64 / 1e6)
                    } else {
                        format!("正在下载 v{version}（{:.1} MB）…", *done as f64 / 1e6)
                    }
                }
            },
            UpdatePhase::ReadyToApply(_) => tr("正在应用更新并重启…", "Applying update and restarting…").to_string(),
            UpdatePhase::Error(e) => {
                if is_en() {
                    format!("Update check failed: {e}")
                } else {
                    format!("更新失败：{e}")
                }
            }
        };
        section_head(ui, tr("关于", "About"), None);
        group(ui, |ui| {
            Item::new(Lead::Icon(ic::INFO), &title).desc(&desc).first(true).show(ui, |ui| match &phase {
                UpdatePhase::Idle => {
                    if Btn::new(tr("检查更新", "Check for Updates")).show(ui).clicked() {
                        self.self_update.start_check(ui.ctx());
                    }
                }
                UpdatePhase::Checking => {
                    Btn::new(tr("检查中…", "Checking…")).enabled(false).show(ui);
                }
                UpdatePhase::UpToDate => {
                    if Btn::new(tr("再次检查", "Check Again")).show(ui).clicked() {
                        self.self_update.start_check(ui.ctx());
                    }
                }
                UpdatePhase::Available(m) => {
                    if Btn::new(tr("重新检查", "Recheck")).show(ui).clicked() {
                        self.self_update.start_check(ui.ctx());
                    }
                    if Btn::new(tr("立即更新", "Update Now")).look(Look::Primary).show(ui).clicked() {
                        if active_session {
                            self.toast(
                                Tone::Warn,
                                tr("当前有远控连接，请先断开后再更新", "Disconnect active remote sessions before updating"),
                            );
                        } else {
                            self.self_update.start_download(ui.ctx(), m.clone());
                        }
                    }
                }
                UpdatePhase::Downloading { .. } | UpdatePhase::ReadyToApply(_) => {
                    Btn::new(tr("更新中…", "Updating…")).enabled(false).show(ui);
                }
                UpdatePhase::Error(_) => {
                    if Btn::new(tr("重试", "Retry")).show(ui).clicked() {
                        self.self_update.start_check(ui.ctx());
                    }
                }
            });
        });
    }

    /// 设置 → 自建中转: the relays this computer owns, tried top to bottom.
    /// A relay code pasted under them adds one; each can go (with 撤回).
    fn relays(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        let job = self.b.job().filter(|j| j.what.starts_with("relay-"));
        let running = job.as_ref().is_some_and(|j| j.answer.is_none());
        if let Some(j) = &job
            && let Some(answer) = &j.answer
        {
            match (j.what.as_str(), answer) {
                ("relay-claim", Ok(url)) => {
                    let host = host_of(url);
                    self.toast(Tone::Ok, if is_en() { format!("Added relay {host}") } else { format!("已添加中转 {host}") });
                    self.adding_relay = false;
                    self.relay_paste.clear();
                }
                (_, Err(e)) => {
                    self.toast(Tone::Err, e.clone());
                }
                _ => {}
            }
            self.b.clear_job();
        }
        let relays: Vec<_> = snap.relays.iter().filter(|r| !self.hidden_relay(&r.url)).cloned().collect();
        let urls: Vec<String> = relays.iter().map(|r| r.url.clone()).collect();
        let visible = self.is_visible(ui.ctx());
        let (mut in_flight, _) = self.relay_probe.snapshot();
        if visible && !urls.is_empty() {
            let need_now = !self.relay_probe.active
                || self.relay_probe.last_urls != urls
                || (!in_flight && self.relay_probe.last_started.is_none_or(|t| t.elapsed() >= RELAY_PROBE_INTERVAL));
            if need_now {
                self.relay_probe.start_wave(ui.ctx(), &urls);
            } else if !in_flight && let Some(t) = self.relay_probe.last_started {
                ui.ctx().request_repaint_after(RELAY_PROBE_INTERVAL.saturating_sub(t.elapsed()));
            }
            in_flight = self.relay_probe.snapshot().0;
        } else {
            self.relay_probe.stop();
            in_flight = false;
        }

        let mut manual_refresh = false;
        section_head_with(
            ui,
            tr("自建中转", "Private Relays"),
            Some(tr(
                "按顺序优先使用，直连受阻时自动接管",
                "Tried in order; takes over automatically when direct P2P fails",
            )),
            |ui| {
                if urls.is_empty() {
                    return;
                }
                let tip = tr("重新测速", "Re-test latency");
                if in_flight {
                    let (rect, r) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, false, tip));
                    glyph(ui.painter(), rect.center(), ic::REFRESH, 13.0, FAINT);
                    r.on_hover_text(tip);
                } else if icon_button(ui, ic::REFRESH, 22.0, 13.0, tip, false)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    manual_refresh = true;
                }
            },
        );
        if manual_refresh && visible && !urls.is_empty() && !in_flight {
            self.relay_probe.start_wave(ui.ctx(), &urls);
        }
        let (_, probe_results) = self.relay_probe.snapshot();
        let relay_hi = self.settings_highlight(ui.ctx(), SettingsTarget::Relays);
        let relay_resp = group_highlighted(ui, relay_hi, |ui| {
            for (i, r) in relays.iter().enumerate() {
                let (state, color) = match r.state.as_str() {
                    "home" => (tr("已连接（首选）", "Connected (Primary)"), GREEN),
                    "temp" => (tr("临时接管（首选不可用）", "Fallback (Primary unavailable)"), GREEN),
                    "fail" => (tr("连接失败", "Connection failed"), AMBER),
                    "idle" => (tr("备用 · 首选不可用时接管", "Standby · takes over if primary fails"), GONE),
                    _ => (tr("正在连接…", "Connecting…"), GONE),
                };
                let desc_suffix = match probe_results.get(&r.url).copied() {
                    Some(ProbeState::Probing) | None => Some((tr(" · 测速中…", " · Testing…").to_string(), DIM)),
                    Some(ProbeState::Ok(d)) => {
                        let ms = d.as_millis().max(1);
                        let sc = if ms >= 200 { AMBER_TEXT } else { DIM };
                        Some((format!(" · {ms} ms"), sc))
                    }
                    Some(ProbeState::Fail) => {
                        if r.state == "fail" {
                            None
                        } else {
                            Some((tr(" · 超时", " · Timed out").to_string(), AMBER_TEXT))
                        }
                    }
                };
                let host = host_of(&r.url);
                let (mut remove, mut pick) = (false, false);
                let line = Item {
                    title_font: FontId::monospace(13.0),
                    desc: Some((state, if color == AMBER { AMBER_TEXT } else { DIM })),
                    desc_suffix,
                    ..Item::new(Lead::Dot(color), &host).first(i == 0)
                };
                line.show(ui, |ui| {
                    let rem_acc = if is_en() { format!("Remove {host}") } else { format!("移除 {host}") };
                    remove = icon_button(ui, ic::DEL, 32.0, 16.0, &rem_acc, true).clicked();
                    if i != 0 {
                        let pick_acc = if is_en() { format!("Set {host} as primary") } else { format!("设为首选 {host}") };
                        pick = Btn::new(tr("设为首选", "Set as Primary"))
                            .look(Look::Ghost)
                            .name(pick_acc)
                            .show(ui)
                            .on_hover_text(tr("以后优先使用这台中转", "Use this relay first"))
                            .clicked();
                    }
                });
                if pick {
                    self.b.act(&format!("relay-use\t{}", r.url));
                    let msg = if is_en() {
                        let note = if snap.codes.is_empty() || r.state == "temp" {
                            ""
                        } else {
                            "; unused connection codes are now void"
                        };
                        format!("Set {host} as primary{note}")
                    } else {
                        let note = if snap.codes.is_empty() || r.state == "temp" {
                            ""
                        } else {
                            "，没用掉的连接码已作废"
                        };
                        format!("已将 {host} 设为首选{note}")
                    };
                    self.toast(Tone::Ok, msg);
                }
                if remove {
                    let msg = if is_en() { format!("Removed relay {host}") } else { format!("已移除中转 {host}") };
                    self.undoable(Undoable::Relay(r.url.clone()), msg);
                }
            }
            let first = relays.is_empty();
            if self.adding_relay {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 76.0), Sense::hover());
                if !first {
                    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(13)));
                }
                let row_top = rect.top() + 12.0;
                glyph(ui.painter(), pos2(rect.left() + 24.0, row_top + 16.0), ic::SERVER, 18.0, DIM);
                let inner = Rect::from_min_size(pos2(rect.left() + 48.0, row_top), vec2(rect.width() - 48.0 - 14.0, 32.0));
                ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let ready = !running && !self.relay_paste.trim().is_empty();
                    let add = if running {
                        Btn::new(tr("添加中…", "Adding…")).enabled(false).show(ui)
                    } else {
                        Btn::new(tr("添加", "Add")).look(Look::Primary).enabled(ready).show(ui)
                    };
                    let cancel = Btn::new(tr("取消", "Cancel")).look(Look::Ghost).show(ui).clicked();
                    let e = ui.add_enabled(
                        !running,
                        TextEdit::singleline(&mut self.relay_paste)
                            .hint_text(tr("粘贴中转码", "Paste relay code"))
                            .desired_width(ui.available_width())
                            .margin(FIELD_MARGIN),
                    );
                    if ui.ctx().memory(|m| m.focused().is_none()) && !running && self.relay_paste.is_empty() {
                        e.request_focus();
                    }
                    let enter = e.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let esc = e.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if cancel || esc {
                        self.adding_relay = false;
                        self.relay_paste.clear();
                    }
                    if (add.clicked() || enter) && ready {
                        self.b.start_job(format!("relay-claim\t{}", self.relay_paste.replace(['\t', '\r', '\n'], " ")));
                    }
                });
                ui.painter().text(
                    pos2(rect.left() + 48.0, rect.top() + 50.0),
                    Align2::LEFT_TOP,
                    tr(
                        "在自建服务器运行 rc-relay ui 获取中转码",
                        "Run rc-relay ui on your relay server to get a relay code",
                    ),
                    font(11.5),
                    DIM,
                );
                ui.advance_cursor_after_rect(rect);
            } else {
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
                let add_label = tr("添加中转服务器", "Add Private Relay");
                r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, add_label));
                if !first {
                    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(13)));
                }
                if r.hovered() {
                    let top = if first { GROUP_RADIUS } else { 0 };
                    let corners = CornerRadius { nw: top, ne: top, sw: GROUP_RADIUS, se: GROUP_RADIUS };
                    ui.painter().rect_filled(rect, corners, veil(6));
                }
                glyph(ui.painter(), pos2(rect.left() + 24.0, rect.center().y), ic::PLUS, 16.0, LINK);
                ui.painter().text(pos2(rect.left() + 48.0, rect.center().y), Align2::LEFT_CENTER, add_label, font(14.0), LINK);
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.adding_relay = true;
                }
            }
        });
        if self.scroll_to_settings == Some(SettingsTarget::Relays) {
            self.scroll_to_settings = None;
            ui.scroll_to_rect(relay_resp.rect, Some(Align::Center));
        }
    }

    /// The toasts, centred at the top of the page: they slide in, stay a
    /// few seconds (longer with 撤回, the clock stopped while the pointer is
    /// on one), and slide out; the ones below move up.
    fn toasts(&mut self, ctx: &egui::Context, area: Rect) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint();
        let max_text = (area.width() - 180.0).clamp(80.0, 420.0);
        let mut y = area.top() + 14.0;
        let mut undone = Vec::new();
        for t in &mut self.toasts {
            let enter = easing::cubic_out((t.born.elapsed().as_secs_f32() / TOAST_IN).min(1.0));
            let exit = t.closing.map_or(0.0, |c| easing::cubic_in((c.elapsed().as_secs_f32() / TOAST_OUT).min(1.0)));
            let ty = ctx.animate_value_with_time(Id::new(("toast-y", t.id)), y, 0.22);
            let pos = pos2(area.center().x, ty - 10.0 * (1.0 - enter) - 10.0 * exit);
            let alpha = enter * (1.0 - exit);
            let (mut close, mut undo) = (false, false);
            let r = egui::Area::new(Id::new(("toast", t.id)))
                .order(Order::Foreground)
                .pivot(Align2::CENTER_TOP)
                .fixed_pos(pos)
                .constrain(false)
                .show(ctx, |ui| {
                    ui.set_opacity(alpha);
                    Frame::new()
                        .fill(RAISED)
                        .stroke(Stroke::new(1.0, veil(18)))
                        .corner_radius(10)
                        .shadow(Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(115) })
                        .inner_margin(Margin { left: 12, right: 6, top: 5, bottom: 5 })
                        .show(ui, |ui| {
                            ui.spacing_mut().interact_size.y = 30.0;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                let (c, color) = tone_look(t.tone);
                                let (r, _) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::hover());
                                glyph(ui.painter(), r.center(), c, 15.0, color);
                                let g = ui.painter().layout(t.text.clone(), font(13.0), TEXT, max_text);
                                let (r, _) = ui.allocate_exact_size(g.size(), Sense::hover());
                                ui.painter().galley(r.min, g, TEXT);
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if t.undo.is_some() {
                                    undo = Btn::new(tr("撤回", "Undo")).look(Look::Link).show(ui).clicked();
                                }
                                close = icon_button(ui, ic::CLOSE, 28.0, 10.0, tr("关闭", "Close"), false).clicked();
                            });
                        });
                });
            let hovered = r.response.contains_pointer();
            if t.closing.is_none() {
                if !hovered {
                    t.shown += dt;
                }
                if undo {
                    t.acted = true;
                    if let Some(id) = t.undo.take() {
                        undone.push(id);
                    }
                }
                if close || undo || t.shown >= t.life {
                    t.closing = Some(now);
                }
            }
            y += (r.response.rect.height() + 8.0) * (1.0 - exit);
        }
        for id in undone {
            self.b.cancel_undoable(id);
        }
        // Gone: a deletion without 撤回 is carried out now.
        let mut gone = Vec::new();
        self.toasts.retain_mut(|t| {
            let done = t.closing.is_some_and(|c| c.elapsed().as_secs_f32() >= TOAST_OUT);
            if done
                && !t.acted
                && let Some(id) = t.undo.take()
            {
                gone.push(id);
            }
            !done
        });
        for id in gone {
            self.b.commit_undoable(id);
        }
    }
}

/// `relay.example.com:7843` of `https://relay.example.com:7843/`.
/// The hosts of a relay's addresses (comma-separated URLs), for showing.
fn host_of(urls: &str) -> String {
    let host = |url: &str| {
        let s = url.split_once("://").map_or(url, |(_, r)| r);
        s.split(['/', '?']).next().unwrap_or(s).to_string()
    };
    urls.split(',').filter(|u| !u.trim().is_empty()).map(|u| host(u.trim())).collect::<Vec<_>>().join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hosts() {
        assert_eq!(short_host("https://relay-a.example.com:7843/,https://203.0.113.7:7843/"), "relay-a");
        assert_eq!(short_host("https://203.0.113.7:7843/"), "203.0.113.7");
        assert_eq!(short_host("https://[2001:db8::7]:7843/"), "2001:db8::7");
    }
}
