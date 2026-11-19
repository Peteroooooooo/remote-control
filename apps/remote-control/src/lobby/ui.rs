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
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontData, FontFamily, FontId, Frame, Galley, Id, Layout, Margin,
    Order, Pos2, Rect, RectAlign, RichText, Sense, Shadow, Stroke, StrokeKind, TextEdit, UiBuilder, Vec2, WidgetInfo,
    WidgetType, emath::easing, pos2, text::LayoutJob, vec2,
};
use iroh::EndpointId;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rc_ident::conn_code::ConnCode;
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    },
};

use super::backend::{Ask, Backend, Dev, SessionState, Snap, Status, Tone};
use crate::{meet, net::PathKind, theme};

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
const MENU_WIDTH: f32 = 210.0;
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Devices,
    Settings,
}

/// A deletion that waits for its toast to go (or for the window to close).
enum Undoable {
    Revoke(String),
    Relay(String),
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

pub struct App {
    b: Backend,
    page: Page,
    paste: String,
    copied: Option<Instant>,
    toasts: Vec<Toast>,
    pending: Vec<(u64, Undoable)>,
    next_id: u64,
    last_frame: Instant,
    /// The node's notice already shown.
    seen_notice: Option<String>,
    cards: Cards,
    /// Whether the service runs another exe than this window, and when that was looked at.
    service_other: Option<(Instant, bool)>,
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
}

/// `hidden`: started at sign-in, with only the icon in the tray.
pub fn run(b: Backend, hidden: bool) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("remote control")
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
            if let Ok(h) = cc.window_handle()
                && let RawWindowHandle::Win32(w) = h.as_raw()
            {
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
                page: Page::Devices,
                paste: String::new(),
                copied: None,
                toasts: Vec::new(),
                pending: Vec::new(),
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
                service_other: None,
                update_later: false,
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
        let none = Color32::TRANSPARENT;
        let (fill, edge, text) = match self.look {
            Look::Normal => (veil(if down { 10 } else if hot { 26 } else { 16 }), veil(13), TEXT),
            Look::Primary => (
                if down {
                    hex(theme::ACCENT_PRESSED)
                } else if hot {
                    hex(theme::ACCENT_HOVER)
                } else {
                    ACCENT
                },
                none,
                Color32::WHITE,
            ),
            Look::Soft if !on => (veil(10), none, DIM),
            Look::Soft => (ACCENT.gamma_multiply(if hot { 0.26 } else { 0.16 }), none, if hot { hex(0xb5d0f7) } else { LINK }),
            Look::Ghost => (if hot { veil(16) } else { none }, none, if hot { TEXT } else { DIM }),
            Look::Danger => (RED.gamma_multiply(if hot { 0.26 } else { 0.16 }), none, DANGER),
            Look::Link => (if hot { ACCENT.gamma_multiply(0.14) } else { none }, none, LINK),
        };
        let text = self.color.unwrap_or(text);
        let fade = if on || self.look == Look::Soft { 1.0 } else { 0.4 };
        let p = ui.painter();
        p.rect(rect, radius, fill.gamma_multiply(fade), Stroke::new(1.0, edge.gamma_multiply(fade)), StrokeKind::Inside);
        let mut x = rect.center().x - (icon_w + g.size().x) / 2.0;
        if let Some(c) = self.icon {
            glyph(p, pos2(x + size / 2.0, rect.center().y), c, size, text.gamma_multiply(fade));
            x += icon_w;
        }
        p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, text.gamma_multiply(fade));
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
    let ring = veil(if hot { 179 } else { 115 }).lerp_to_gamma(ACCENT, t);
    let fill = Color32::TRANSPARENT.lerp_to_gamma(ACCENT, t);
    ui.painter().rect(rect, 10.0, fill.gamma_multiply(fade), Stroke::new(1.0, ring.gamma_multiply(fade)), StrokeKind::Inside);
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 11.0, t);
    let knob = hex(0xc9cbd0).lerp_to_gamma(Color32::WHITE, t).gamma_multiply(fade);
    ui.painter().circle_filled(pos2(x, rect.center().y), if hot { 7.0 } else { 6.0 }, knob);
    r
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
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                match lead {
                    Lead::Icon(c) => glyph(ui.painter(), r.center(), c, 18.0, fg),
                    Lead::Spin => spin(ui, r.center(), 7.0, 2.0, LINK),
                    Lead::Dot(c) => {
                        ui.painter().circle_filled(r.center(), 4.0, c);
                    }
                }
                ui.label(RichText::new(text).size(14.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    buttons(ui)
                });
            });
        });
}

/// A section's title, with a note after it.
fn section_head(ui: &mut egui::Ui, title: &str, note: Option<&str>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(bold(13.0)));
        if let Some(n) = note {
            ui.label(RichText::new(n).size(12.0).color(DIM));
        }
    });
}

/// The card that holds a section's rows or lines.
fn group(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    Frame::new().fill(CARD).corner_radius(GROUP_RADIUS).stroke(Stroke::new(1.0, veil(8))).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
    });
}

/// One line of a settings card: an icon (or a dot), a title, a line under
/// it, the controls on the right, all centred on the line.
struct Item<'a> {
    title: &'a str,
    title_font: FontId,
    desc: Option<(&'a str, Color32)>,
    lead: Lead,
    /// The card's first line: no divider above.
    first: bool,
    enabled: bool,
}

impl<'a> Item<'a> {
    fn new(lead: Lead, title: &'a str) -> Self {
        Item { title, title_font: font(14.0), desc: None, lead, first: false, enabled: true }
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

    fn show(self, ui: &mut egui::Ui, right: impl FnOnce(&mut egui::Ui)) {
        const PAD: Vec2 = Vec2::new(14.0, 10.0);
        /// Kept for the controls on the right.
        const RIGHT: f32 = 260.0;
        let w = ui.available_width();
        let indent = 34.0;
        let text_w = (w - 2.0 * PAD.x - RIGHT - indent).max(160.0);
        let fade = if self.enabled { 1.0 } else { 0.45 };
        let title = ui.painter().layout(self.title.to_string(), self.title_font, TEXT.gamma_multiply(fade), text_w);
        let desc = self.desc.map(|(d, c)| ui.painter().layout(d.to_string(), font(12.0), c.gamma_multiply(fade), text_w));
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
        let right_rect = Rect::from_min_max(pos2(rect.right() - PAD.x - RIGHT, rect.top()), pos2(rect.right() - PAD.x, rect.bottom()));
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
    let p = ui.painter();
    let fill = if on {
        veil(16)
    } else if r.hovered() {
        veil(10)
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CONTROL_RADIUS, fill);
    if on {
        let bar = Rect::from_min_size(pos2(rect.left(), rect.top() + 10.0), vec2(3.0, 16.0));
        p.rect_filled(bar, 2.0, ACCENT);
    }
    glyph(p, pos2(rect.left() + 20.0, rect.center().y), c, 16.0, if on { TEXT } else { DIM });
    p.text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, text, font(14.0), TEXT);
    if let Some(n) = count {
        p.text(pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, n.to_string(), font(12.0), DIM);
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One line of a popup menu: clickable across its width, lit while the
/// pointer is on it (red for `danger`). `sub`: dim text after the title;
/// `tick`: the choice in force.
fn menu_item(ui: &mut egui::Ui, icon: Option<char>, text: &str, sub: Option<&str>, tick: bool, danger: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    r.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, tick, text));
    let t = ui.ctx().animate_bool_with_time(r.id, ui.rect_contains_pointer(rect), 0.1);
    let p = ui.painter();
    if t > 0.0 {
        let fill = if danger { RED.gamma_multiply(0.16 * t) } else { veil(16).gamma_multiply(t) };
        p.rect_filled(rect, CONTROL_RADIUS, fill);
    }
    let color = if danger { DANGER } else { TEXT };
    if let Some(c) = icon {
        glyph(p, pos2(rect.left() + 18.0, rect.center().y), c, 14.0, if danger { DANGER } else { DIM });
    }
    let g = p.layout_no_wrap(text.to_string(), font(14.0), color);
    let x = rect.left() + 36.0;
    let w = g.size().x;
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, color);
    if let Some(s) = sub {
        p.text(pos2(x + w + 8.0, rect.center().y), Align2::LEFT_CENTER, s, font(12.0), DIM);
    }
    if tick {
        glyph(p, pos2(rect.right() - 16.0, rect.center().y), ic::CHECK, 12.0, LINK);
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
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
    match s {
        0..60 => "刚才".into(),
        60..3600 => format!("{} 分钟前", s / 60),
        _ => format!("{} 小时前", s / 3600),
    }
}

/// The line under a device's name: its colour, and whether something is
/// going on (a spinner before it).
fn sub_line(d: &Dev, session: Option<SessionState>) -> (String, Color32, bool) {
    if d.ask == Ask::In {
        return ("想记住你。同意后互相记住，以后随时能连".into(), AMBER_TEXT, false);
    }
    match session {
        Some(SessionState::Connecting) => return ("正在连接…".into(), DIM, true),
        Some(SessionState::Waiting) => return ("等对方允许".into(), DIM, true),
        Some(SessionState::Connected) => return ("控制中".into(), LINK, false),
        Some(SessionState::Paused) => return ("控制中 · 画面暂停".into(), AMBER_TEXT, false),
        None => {}
    }
    if d.ask == Ask::Out {
        return ("等对方同意".into(), DIM, true);
    }
    let via = match d.met.as_str() {
        _ if d.listed => "",
        "code" => "连接码 · ",
        "lan" => "局域网 · ",
        _ => "",
    };
    let (s, color, busy) = match d.status {
        Status::Online { rtt_ms, path } => {
            let way = match path {
                PathKind::Direct => " · 直连",
                PathKind::Relay => " · 经自建中转",
                PathKind::Public => " · 正在找直连",
                PathKind::None => "",
            };
            (format!("在线 · {:.0} ms{way}", rtt_ms.max(1.0)), DIM, false)
        }
        Status::Checking => ("正在找…".into(), DIM, true),
        Status::Offline => ("离线".into(), DIM, false),
        Status::Unreachable => ("打不通".into(), AMBER_TEXT, false),
        Status::Refused(r) => (meet::refusal_text(r).into(), AMBER_TEXT, false),
    };
    (format!("{via}{s}"), color, busy)
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
        for (_, what) in std::mem::take(&mut self.pending) {
            self.carry_out(what);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray();
        }
        let snap = self.b.snap();
        self.take_notices(&snap);
        if self.confirm.is_none() {
            self.paste_anywhere(&ctx);
        }
        let esc = ctx.memory(|m| m.focused().is_none()) && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if esc && self.confirm.is_some() {
            self.confirm = None;
        } else if esc && self.page == Page::Settings {
            self.page = Page::Devices;
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
        let content = egui::CentralPanel::default()
            .frame(layer)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Room for the scroll bar on the right, and above and below the rows
                    // (inside the scrolled part, so rows scroll up to the edge).
                    ui.set_width(ui.available_width() - 18.0);
                    ui.add_space(21.0);
                    match self.page {
                        Page::Devices => self.devices(ui, &snap),
                        Page::Settings => self.settings(ui, &snap),
                    }
                    ui.add_space(16.0);
                });
            })
            .response
            .rect;
        self.toasts(&ctx, content);
        self.confirm_dialog(&ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl App {
    /// Closing the window: it goes to the tray, and counts as closed.
    fn hide_to_tray(&mut self) {
        for (_, what) in std::mem::take(&mut self.pending) {
            self.carry_out(what);
        }
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
        self.pending.push((id, what));
        self.push_toast(Tone::Info, text, Some(id));
    }

    fn carry_out(&self, what: Undoable) {
        match what {
            Undoable::Revoke(token) => self.b.act(&format!("revoke\t{token}")),
            Undoable::Relay(url) => self.b.act(&format!("relay-remove\t{url}")),
        }
    }

    /// Deleting a device takes effect at once, on both sides: waiting for an
    /// undo would leave the other side showing it for as long.
    fn delete(&mut self, c: Confirm) {
        if self.renaming.as_ref().is_some_and(|(id, _)| *id == c.id) {
            self.renaming = None;
        }
        self.b.act(&format!("remove\t{}", c.id));
        let there = if c.there { "对方那边也删掉了本机" } else { "对方上线时也会删掉本机" };
        self.toast(Tone::Ok, format!("已删除「{}」，{there}", c.name));
    }

    fn hidden_code(&self, token: &str) -> bool {
        self.pending.iter().any(|(_, w)| matches!(w, Undoable::Revoke(t) if t == token))
    }

    fn hidden_relay(&self, url: &str) -> bool {
        self.pending.iter().any(|(_, w)| matches!(w, Undoable::Relay(u) if u == url))
    }

    /// Messages of the node and of the backend become toasts.
    fn take_notices(&mut self, snap: &Snap) {
        if let Some((n, text)) = &snap.notice
            && self.seen_notice.as_ref() != Some(n)
        {
            self.seen_notice = Some(n.clone());
            self.b.act(&format!("dismiss\t{n}"));
            let tone = if text.contains("互相记住") { Tone::Ok } else { Tone::Warn };
            self.toast(tone, text.clone());
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
                self.page = Page::Devices;
                // The card may have been there already (found on the LAN): say it went in.
                let snap = self.b.snap();
                let dev = snap.devs.iter().find(|d| d.id.to_string() == id.trim());
                let text = match dev {
                    Some(d) if matches!(d.status, Status::Online { .. }) => format!("已和「{}」连上", d.name),
                    Some(d) => format!("正在连接「{}」…", d.name),
                    None => "正在连接对方…".to_string(),
                };
                self.toast(Tone::Info, text);
            }
            Err(e) => self.toast(Tone::Err, format!("{e:#}")),
        }
    }

    fn left(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        ui.spacing_mut().item_spacing.y = 2.0;
        // This computer.
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::hover());
        let av = Rect::from_min_size(rect.min + vec2(8.0, 6.0), Vec2::splat(40.0));
        let p = ui.painter();
        p.rect_filled(av, 10.0, ACCENT.gamma_multiply(0.16));
        glyph(p, av.center(), ic::DESKTOP, 20.0, LINK);
        let x = av.right() + 12.0;
        let name = if snap.name.is_empty() { "…" } else { &snap.name };
        let g = one_line(ui, name, bold(16.0), TEXT, rect.right() - x);
        let top = av.center().y - (g.size().y + 17.0) / 2.0;
        let gh = g.size().y;
        ui.painter().galley(pos2(x, top), g, TEXT);
        let (dot, state) = if snap.me.is_empty() { (GONE, "本机 · 正在启动") } else { (GREEN, "本机 · 在线") };
        let y = top + gh + 9.0;
        ui.painter().circle_filled(pos2(x + 3.5, y), 3.5, dot);
        ui.painter().text(pos2(x + 13.0, y), Align2::LEFT_CENTER, state, font(12.0), DIM);

        if nav(ui, ic::DEVICES, "设备", self.page == Page::Devices, Some(snap.devs.len())).clicked() {
            self.page = Page::Devices;
        }
        if nav(ui, ic::GEAR, "设置", self.page == Page::Settings, None).clicked() {
            self.page = Page::Settings;
        }

        let relays: Vec<_> = snap.relays.iter().filter(|r| !self.hidden_relay(&r.url)).collect();
        if relays.is_empty() {
            return;
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for r in relays.iter().rev() {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
                let (color, state) = match r.state.as_str() {
                    "home" => (GREEN, "在用"),
                    "temp" => (GREEN, "临时在用"),
                    "fail" => (AMBER, "连不上"),
                    "idle" => (GONE, "备用"),
                    _ => (GONE, "…"),
                };
                let p = ui.painter();
                p.circle_filled(pos2(rect.left() + 11.5, rect.center().y), 3.5, color);
                let right = p.layout_no_wrap(state.to_string(), font(12.0), DIM);
                let rw = right.size().x;
                p.galley(pos2(rect.right() - 8.0 - rw, rect.center().y - right.size().y / 2.0), right, DIM);
                let host = one_line(ui, &short_host(&r.url), font(12.0), TEXT, rect.width() - 23.0 - rw - 16.0);
                let hy = rect.center().y - host.size().y / 2.0;
                ui.painter().galley(pos2(rect.left() + 23.0, hy), host, TEXT);
            }
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
            ui.painter().text(pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, "自建中转", font(12.0), DIM);
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
            self.cards.hosting = format!("「{n}」正在控制本机");
        }
        let text = self.cards.hosting.clone();
        let b = self.b.clone();
        reveal(ui, Id::new("card-hosting"), snap.hosting.is_some(), GAP, |ui| {
            banner(ui, Tint::Red, Lead::Icon(ic::DESKTOP), &text, |ui| {
                if Btn::new("断开").look(Look::Danger).show(ui).clicked() {
                    b.act("disconnect");
                }
            })
        });

        if let Some(n) = &snap.asking {
            self.cards.asking = format!("「{n}」请求控制本机，在弹窗里允许或拒绝");
        }
        let text = self.cards.asking.clone();
        reveal(ui, Id::new("card-asking"), snap.asking.is_some(), GAP, |ui| {
            banner(ui, Tint::Amber, Lead::Icon(ic::SHIELD), &text, |_| {})
        });

        let suggest = snap.suggest.is_some() && snap.hosting.is_none();
        let idle = self.b.busy().is_none();
        reveal(ui, Id::new("card-suggest"), suggest, GAP, |ui| {
            banner(ui, Tint::Blue, Lead::Icon(ic::SERVICE), "装上后台服务，锁屏时也能被控制", |ui| {
                if Btn::new("安装").look(Look::Primary).enabled(idle).show(ui).clicked() {
                    b.act("unsuggest");
                    b.service(true);
                }
                if Btn::new("以后").look(Look::Ghost).show(ui).clicked() {
                    b.act("unsuggest");
                }
            })
        });

        let other = self.service_other() && !self.update_later;
        let mut later = false;
        reveal(ui, Id::new("card-update"), other, GAP, |ui| {
            banner(ui, Tint::Blue, Lead::Icon(ic::SERVICE), "后台服务用的是另一个位置的程序，改用这个", |ui| {
                if Btn::new("改用这个").look(Look::Primary).enabled(idle).show(ui).clicked() {
                    b.service(true);
                }
                later = Btn::new("以后").look(Look::Ghost).show(ui).clicked();
            })
        });
        self.update_later |= later;
    }

    /// Whether the service runs another exe (`install::service_elsewhere`),
    /// looked at every few seconds.
    fn service_other(&mut self) -> bool {
        if self.service_other.is_none_or(|(t, _)| t.elapsed() >= Duration::from_secs(5)) {
            let other = self.b.service_installed() && crate::install::service_elsewhere().is_some();
            self.service_other = Some((Instant::now(), other));
        }
        self.service_other.is_some_and(|(_, other)| other)
    }

    fn devices(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.label(RichText::new("设备").font(bold(22.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let done = self.copied.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                if done {
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                    Btn::new("已复制，1 小时内有效").icon(ic::CHECK).color(GREEN).show(ui);
                } else if Btn::new("复制本机连接码").icon(ic::COPY).enabled(!snap.me.is_empty()).show(ui).clicked() {
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
        let codes: Vec<_> = snap.codes.iter().filter(|c| !self.hidden_code(&c.token)).cloned().collect();
        if !codes.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                ui.spacing_mut().interact_size.y = 26.0;
                ui.label(RichText::new("还没用掉的连接码").size(12.0).color(DIM));
                for c in codes {
                    Frame::new().fill(veil(13)).corner_radius(13).inner_margin(Margin { left: 10, right: 2, top: 2, bottom: 2 }).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.spacing_mut().interact_size.y = 22.0;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}复制", ago(c.age_s))).size(12.0));
                            let r = Btn::new("撤销").look(Look::Ghost).small().name(format!("撤销 {}复制的连接码", ago(c.age_s))).show(ui);
                            if r.clicked() {
                                self.undoable(Undoable::Revoke(c.token.clone()), "已撤销连接码".into());
                            }
                        });
                    });
                }
            });
        }
        ui.add_space(14.0);
        self.banners(ui, snap);

        let sessions = self.b.sessions();
        type Kind = (&'static str, &'static str, fn(&Dev) -> bool);
        let kinds: [Kind; 3] = [
            ("我的电脑", "控制本机不用问", |d| d.listed && d.auto),
            ("信任设备", "控制本机要先问", |d| d.listed && !d.auto),
            ("临时设备", "控制本机要先问，关窗口就断开", |d| !d.listed),
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
                    self.row(ui, d, sessions.get(&d.id).map(|s| s.1), i == 0, i == last);
                }
            });
            ui.add_space(12.0);
        }
        if !any && !snap.me.is_empty() {
            ui.add_space(56.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(ic::DEVICES.to_string()).font(FontId::new(40.0, FontFamily::Name(ICONS.into()))).color(FAINT));
                ui.add_space(4.0);
                ui.label("还没有设备");
                ui.label(RichText::new("复制连接码发给对方，或把对方的连接码粘贴到上面").size(12.0).color(DIM));
            });
        }
    }

    /// Where the other side's connection code goes: a code goes in as soon
    /// as it is pasted (or typed whole).
    fn connect_field(&mut self, ui: &mut egui::Ui) {
        let id = Id::new("connect");
        let focused = ui.memory(|m| m.has_focus(id));
        let mut submit = None;
        let frame = Frame::new()
            .fill(veil(10))
            .stroke(Stroke::new(1.0, if focused { ACCENT } else { veil(16) }))
            .corner_radius(8)
            .inner_margin(Margin { left: 14, right: 6, top: 0, bottom: 0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().interact_size.y = 40.0;
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                    glyph(ui.painter(), r.center(), ic::LINK, 16.0, DIM);
                    let kbd = if self.paste.is_empty() { 64.0 } else { 0.0 };
                    let e = ui.add(
                        TextEdit::singleline(&mut self.paste)
                            .id(id)
                            .frame(Frame::NONE)
                            .hint_text(RichText::new("粘贴对方的连接码").color(FAINT))
                            .font(font(14.0))
                            .margin(Margin::symmetric(0, 4))
                            .desired_width(ui.available_width() - kbd),
                    );
                    if e.changed() && ConnCode::find(&self.paste).is_some() {
                        submit = Some(self.paste.clone());
                    }
                    if e.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.paste.trim().is_empty() {
                        submit = Some(self.paste.clone());
                    }
                    if self.paste.is_empty() {
                        let g = ui.painter().layout_no_wrap("Ctrl V".into(), font(11.0), DIM);
                        let (r, _) = ui.allocate_exact_size(g.size() + vec2(12.0, 4.0), Sense::hover());
                        ui.painter().rect_stroke(r, 4.0, Stroke::new(1.0, veil(23)), StrokeKind::Inside);
                        ui.painter().galley(r.center() - g.size() / 2.0, g, DIM);
                    }
                });
            });
        if frame.response.interact(Sense::click()).clicked() {
            ui.memory_mut(|m| m.request_focus(id));
        }
        if let Some(t) = submit {
            self.submit_code(&t);
        }
    }

    /// One device: its picture (what it is to this computer, a dot for its
    /// status), its name and what is going on, its buttons. Double-click:
    /// control it; double-click the name of a remembered one: rename it.
    fn row(&mut self, ui: &mut egui::Ui, d: &Dev, session: Option<SessionState>, first: bool, last: bool) {
        let online = matches!(d.status, Status::Online { .. });
        let unreachable = d.status == Status::Unreachable;
        let asked = d.ask == Ask::In;
        let why_open = unreachable && self.why == Some(d.id);
        let (rect, row) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
        if online && session.is_none() && !asked && row.double_clicked() {
            self.b.control(d.id, &d.name);
        }
        let p = ui.painter().clone();
        let top = if first { GROUP_RADIUS } else { 0 };
        let bottom = if last && !why_open { GROUP_RADIUS } else { 0 };
        let corners = CornerRadius { nw: top, ne: top, sw: bottom, se: bottom };
        if asked {
            p.rect_filled(rect, corners, AMBER.gamma_multiply(0.07));
            let bar = Rect::from_min_max(pos2(rect.left(), rect.top() + 10.0), pos2(rect.left() + 3.0, rect.bottom() - 10.0));
            p.rect_filled(bar, 2.0, AMBER);
        } else if row.hovered() {
            p.rect_filled(rect, corners, veil(6));
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
            (GREEN.gamma_multiply(0.13), hex(0x7fd38b))
        };
        p.rect_filled(av, 9.0, fill);
        glyph(&p, av.center(), ic::DESKTOP, 18.0, fg);
        let dot = av.right_bottom() - vec2(3.0, 3.0);
        p.circle_filled(dot, 6.0, CARD);
        p.circle_filled(dot, 4.0, if online { GREEN } else if unreachable { AMBER } else { GONE });

        // The buttons, from the right.
        let acts_rect = Rect::from_min_max(pos2(rect.left() + ROW_TEXT, rect.top()), pos2(rect.right() - 10.0, rect.bottom()));
        let mut acts = ui.new_child(UiBuilder::new().max_rect(acts_rect).layout(Layout::right_to_left(Align::Center)));
        acts.spacing_mut().item_spacing.x = 6.0;
        self.row_menu(&mut acts, d);
        if asked {
            if Btn::new("同意").look(Look::Primary).name(format!("同意 {}", d.name)).show(&mut acts).clicked() {
                match self.b.request(&format!("agree\t{}", d.id)) {
                    Ok(_) => self.toast(Tone::Ok, format!("已和「{}」互相记住", d.name)),
                    Err(e) => self.toast(Tone::Err, format!("{e:#}")),
                }
            }
            if Btn::new("拒绝").look(Look::Ghost).name(format!("拒绝 {}", d.name)).show(&mut acts).clicked() {
                self.b.act(&format!("refuse\t{}", d.id));
            }
        } else {
            match session {
                Some(_) => {}
                None if unreachable => {
                    if Btn::new("为什么").look(Look::Ghost).name(format!("为什么 {}", d.name)).show(&mut acts).clicked() {
                        self.why = if self.why == Some(d.id) { None } else { Some(d.id) };
                    }
                }
                None => {
                    let r = Btn::new("控制").look(Look::Soft).enabled(online).name(format!("控制 {}", d.name)).show(&mut acts);
                    if r.clicked() {
                        self.b.control(d.id, &d.name);
                    }
                }
            }
            if !d.listed && d.ask == Ask::None {
                let r = Btn::new("记住").look(Look::Ghost).icon(ic::ADD).name(format!("记住 {}", d.name)).show(&mut acts);
                if r.clicked() {
                    match self.b.request(&format!("remember\t{}", d.id)) {
                        Ok(both) if both.trim() == "both" => self.toast(Tone::Ok, format!("已和「{}」互相记住", d.name)),
                        Ok(_) => {}
                        Err(e) => self.toast(Tone::Err, format!("{e:#}")),
                    }
                }
            }
        }
        let text_right = acts.min_rect().left() - 12.0;

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
                if !text.trim().is_empty() && text.trim() != d.name {
                    self.b.act(&format!("rename\t{id}\t{}", text.replace(['\t', '\n'], " ")));
                }
            }
        } else {
            let at = Rect::from_min_size(pos2(x, y), name.size());
            p.galley(at.min, name, TEXT);
            if d.listed {
                let r = ui.interact(at, Id::new(("name", d.id)), Sense::click()).on_hover_text("双击改名");
                if r.double_clicked() {
                    self.renaming = Some((d.id, d.name.clone()));
                }
            }
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
                ui.label(RichText::new("找不到直连。打开路由器的 IPv6，或者加一台自建中转。").size(13.0).color(DIM));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if Btn::new("重试").show(ui).clicked() {
                        b.act(&format!("retry\t{}", d.id));
                    }
                    to_settings = Btn::new("去设置").show(ui).clicked();
                });
            });
        });
        if to_settings {
            self.why = None;
            self.page = Page::Settings;
        }
    }

    fn row_menu(&mut self, ui: &mut egui::Ui, d: &Dev) {
        let more = icon_button(ui, ic::MORE, 32.0, 16.0, &format!("更多 {}", d.name), false);
        egui::Popup::menu(&more).align(RectAlign::BOTTOM_END).gap(4.0).show(|ui| {
            ui.set_width(MENU_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;
            if menu_item(ui, Some(ic::TERM), "终端", None, false, false).clicked() {
                ui.close();
                if let Err(e) = crate::term::open_window(d.id, &d.name) {
                    self.toast(Tone::Err, format!("打不开终端：{e:#}"));
                }
            }
            if d.listed {
                if menu_item(ui, Some(ic::RENAME), "重命名", None, false, false).clicked() {
                    self.renaming = Some((d.id, d.name.clone()));
                    ui.close();
                }
                menu_sep(ui);
                menu_head(ui, "它控制本机时");
                for (auto, title, sub) in [(true, "我的电脑", "不用问"), (false, "信任设备", "先问")] {
                    if menu_item(ui, None, title, Some(sub), d.auto == auto, false).clicked() {
                        ui.close();
                        if d.auto != auto {
                            self.b.act(&format!("auto\t{}\t{}", d.id, auto as u8));
                            let how = if auto { "不用问" } else { "要先问" };
                            self.toast(Tone::Ok, format!("「{}」以后控制本机{how}", d.name));
                        }
                    }
                }
            }
            menu_sep(ui);
            if menu_item(ui, Some(ic::DEL), "删除", None, false, true).clicked() {
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
                            ui.add(egui::Label::new(RichText::new(format!("删除「{}」？", c.name)).font(bold(16.0))).wrap());
                            ui.add(
                                egui::Label::new(
                                    RichText::new("两台电脑都会忘掉对方，本机中转马上停止为它转发。以后要再连，重新发连接码。")
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
                        cancel = Btn::new("取消").min_width(84.0).show(ui).clicked();
                        delete = Btn::new("删除").look(Look::Danger).min_width(84.0).show(ui).clicked();
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
            ui.label(RichText::new("设置").font(bold(22.0)));
        });
        ui.add_space(6.0);
        let b = self.b.clone();

        section_head(ui, "本机", None);
        group(ui, |ui| {
            let name = &mut self.name;
            let saved = &mut self.saved;
            Item::new(Lead::Icon(ic::RENAME), "名字").desc("对方看到的名字").first(true).show(ui, |ui| {
                let e = ui.add(TextEdit::singleline(name).hint_text(&snap.name).desired_width(200.0).margin(FIELD_MARGIN));
                if !e.has_focus() && !e.lost_focus() {
                    name.clone_from(&snap.set_name);
                }
                if e.lost_focus() && name.trim() != snap.set_name {
                    b.act(&format!("set-name\t{}", name.replace(['\t', '\n'], " ")));
                    *saved = Some(Instant::now());
                }
                if let Some(t) = *saved {
                    let a = 1.0 - ((t.elapsed().as_secs_f32() - 1.4) / 0.3).clamp(0.0, 1.0);
                    if a > 0.0 {
                        ui.ctx().request_repaint();
                        ui.label(RichText::new("✓ 已保存").size(12.0).color(GREEN.gamma_multiply(a)));
                    }
                }
            });
        });

        ui.add_space(12.0);
        section_head(ui, "连接", None);
        group(ui, |ui| {
            let mut public = snap.public;
            Item::new(Lead::Icon(ic::GLOBE), "公共节点牵线").desc("只交换地址，不传画面").first(true).show(ui, |ui| {
                if switch(ui, &mut public, true, "公共节点牵线").changed() {
                    b.act(&format!("set\tpublic\t{}", public as u8));
                }
            });
            let mut lan = snap.lan;
            Item::new(Lead::Icon(ic::WIFI), "局域网可见").desc("同一个局域网里的电脑能看到本机").show(ui, |ui| {
                if switch(ui, &mut lan, true, "局域网可见").changed() {
                    b.act(&format!("set\tlan\t{}", lan as u8));
                }
            });
        });

        ui.add_space(12.0);
        self.relays(ui, snap);

        ui.add_space(12.0);
        let other = self.service_other();
        section_head(ui, "后台服务", None);
        group(ui, |ui| {
            let installed = b.service_installed();
            let busy = b.busy();
            let (title, desc) = match (installed, snap.service) {
                (true, true) if other => ("运行中，用的是另一个位置的程序", "改用这个才用上这一版"),
                (true, true) => ("运行中", "关掉窗口也在线"),
                (true, false) => ("已安装，没在运行", "装上后锁屏时也能被控制"),
                _ => ("没装", "装上后锁屏时也能被控制"),
            };
            Item::new(Lead::Icon(ic::SERVICE), title).desc(desc).first(true).show(ui, |ui| {
                if let Some(what) = &busy {
                    let label = if what.contains("卸载") { "卸载中…" } else { "安装中…" };
                    Btn::new(label).enabled(false).show(ui);
                } else if !installed {
                    if Btn::new("安装").look(Look::Primary).show(ui).clicked() {
                        b.service(true);
                    }
                } else {
                    if installed && other && Btn::new("改用这个").look(Look::Primary).show(ui).clicked() {
                        b.service(true);
                    }
                    if Btn::new("卸载").show(ui).clicked() {
                        b.service(false);
                    }
                }
            });
            let on = snap.service;
            for (key, icon, title, desc, value) in [
                ("boot", ic::POWER, "开机自动在线", None, snap.boot),
                ("lock", ic::LOCK, "锁屏时可被控制", Some("仅限我的电脑"), snap.lock),
                ("awake", ic::BOLT, "插电时不睡眠", Some("屏幕照常熄灭"), snap.awake),
            ] {
                let mut v = value;
                let line = Item::new(Lead::Icon(icon), title).enabled(on);
                let line = match desc {
                    Some(d) => line.desc(d),
                    None => line,
                };
                line.show(ui, |ui| {
                    if switch(ui, &mut v, on, title).changed() {
                        b.act(&format!("set\t{key}\t{}", v as u8));
                    }
                });
            }
        });

        ui.add_space(12.0);
        let mut taught = None;
        section_head(ui, "AI", None);
        group(ui, |ui| {
            Item::new(Lead::Icon(ic::BOT), "让本机的 AI 会用")
                .desc("Claude Code、Codex 学会用终端操作设备列表里的电脑")
                .first(true)
                .show(ui, |ui| {
                    if Btn::new("写入").show(ui).clicked() {
                        taught = Some(crate::term::install_skill());
                    }
                });
        });
        match taught {
            Some(Ok(where_)) => self.toast(Tone::Ok, format!("已写入 {where_}，新开的 AI 对话就会用")),
            Some(Err(e)) => self.toast(Tone::Err, format!("没写进去：{e:#}")),
            None => {}
        }
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
                    self.toast(Tone::Ok, format!("已加上中转 {host}"));
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
        section_head(ui, "自建中转", Some("从上往下用，第一台连不上时下一台临时顶替"));
        group(ui, |ui| {
            for (i, r) in relays.iter().enumerate() {
                let (state, color) = match r.state.as_str() {
                    "home" => ("在用", GREEN),
                    "temp" => ("临时在用（首选连不上）", GREEN),
                    "fail" => ("连不上", AMBER),
                    "idle" => ("未使用，名单照常同步", GONE),
                    _ => ("正在连接…", GONE),
                };
                let host = host_of(&r.url);
                let (mut remove, mut pick) = (false, false);
                let line = Item {
                    title_font: FontId::monospace(13.0),
                    desc: Some((state, if color == AMBER { AMBER_TEXT } else { DIM })),
                    ..Item::new(Lead::Dot(color), &host).first(i == 0)
                };
                line.show(ui, |ui| {
                    remove = icon_button(ui, ic::DEL, 32.0, 16.0, &format!("移除 {host}"), true).clicked();
                    if i != 0 {
                        pick = Btn::new("使用").look(Look::Ghost).name(format!("使用 {host}")).show(ui).on_hover_text("以后优先用这台中转").clicked();
                    }
                });
                if pick {
                    self.b.act(&format!("relay-use\t{}", r.url));
                    let note = if snap.codes.is_empty() || r.state == "temp" { "" } else { "，没用掉的连接码已作废" };
                    self.toast(Tone::Ok, format!("已改用中转 {host}{note}"));
                }
                if remove {
                    self.undoable(Undoable::Relay(r.url.clone()), format!("已移除中转 {host}"));
                }
            }
            let first = relays.is_empty();
            if self.adding_relay {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 62.0), Sense::hover());
                if !first {
                    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(13)));
                }
                glyph(ui.painter(), pos2(rect.left() + 24.0, rect.center().y), ic::SERVER, 18.0, DIM);
                let inner = Rect::from_min_max(pos2(rect.left() + 48.0, rect.top()), pos2(rect.right() - 14.0, rect.bottom()));
                ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let ready = !running && !self.relay_paste.trim().is_empty();
                    let add = if running {
                        Btn::new("正在添加…").enabled(false).show(ui)
                    } else {
                        Btn::new("添加").look(Look::Primary).enabled(ready).show(ui)
                    };
                    let cancel = Btn::new("取消").look(Look::Ghost).show(ui).clicked();
                    let e = ui.add_enabled(
                        !running,
                        TextEdit::singleline(&mut self.relay_paste).hint_text("粘贴中转码").desired_width(ui.available_width()).margin(FIELD_MARGIN),
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
                ui.advance_cursor_after_rect(rect);
            } else {
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
                r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "添加中转"));
                if !first {
                    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(13)));
                }
                if r.hovered() {
                    let top = if first { GROUP_RADIUS } else { 0 };
                    let corners = CornerRadius { nw: top, ne: top, sw: GROUP_RADIUS, se: GROUP_RADIUS };
                    ui.painter().rect_filled(rect, corners, veil(6));
                }
                glyph(ui.painter(), pos2(rect.left() + 24.0, rect.center().y), ic::PLUS, 16.0, LINK);
                ui.painter().text(pos2(rect.left() + 48.0, rect.center().y), Align2::LEFT_CENTER, "添加中转", font(14.0), LINK);
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.adding_relay = true;
                }
            }
        });
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
        let max_text = (area.width() - 160.0).clamp(160.0, 420.0);
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
                                    undo = Btn::new("撤回").look(Look::Link).show(ui).clicked();
                                }
                                close = icon_button(ui, ic::CLOSE, 28.0, 10.0, "关闭", false).clicked();
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
            self.pending.retain(|(p, _)| *p != id);
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
            if let Some(i) = self.pending.iter().position(|(p, _)| *p == id) {
                let (_, what) = self.pending.remove(i);
                self.carry_out(what);
            }
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
        assert_eq!(short_host("https://sg-relay.example.com:7843/,https://203.0.113.7:7843/"), "sg-relay");
        assert_eq!(short_host("https://203.0.113.7:7843/"), "203.0.113.7");
        assert_eq!(short_host("https://[2001:db8::7]:7843/"), "2001:db8::7");
    }
}
