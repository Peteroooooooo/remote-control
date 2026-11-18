//! The lobby window (egui), two panes (`target-plan` 2.3): this computer on
//! the left (its name, the connection code, pasting one, unused codes, the
//! page switch), a page on the right: the devices ("我的设备", then "附近和
//! 临时") or the settings. Messages come and go as toasts at the top of the
//! page; deleting something can be taken back while its toast shows (the
//! request goes out when the toast is gone). It only draws `backend::Snap`
//! and calls `Backend`.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Atom, Color32, CornerRadius, FontData, FontFamily, FontId, Frame, Id, Layout, Margin, Order,
    Rect, RectAlign, RichText, Sense, Shadow, Stroke, StrokeKind, TextEdit, UiBuilder, Vec2, WidgetInfo, WidgetType,
    emath::easing, pos2, vec2,
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

use super::backend::{Backend, Dev, SessionState, Snap, Status, Tone, clipboard_text};
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
const ROW: Color32 = hex(theme::CARD);
const ROW_HOVER: Color32 = hex(0x2e3035);
const RAISED: Color32 = hex(theme::RAISED);
const TEXT: Color32 = hex(theme::TEXT);
const DIM: Color32 = hex(theme::DIM);
const GREEN: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x50);
/// The copy button right after copying.
const DONE: Color32 = Color32::from_rgb(0x2e, 0x8a, 0x3e);
const AMBER: Color32 = Color32::from_rgb(0xd2, 0x99, 0x22);
const RED: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
/// Text of buttons that end or delete something.
const DANGER: Color32 = Color32::from_rgb(0xff, 0x8a, 0x83);
const ACCENT: Color32 = hex(theme::ACCENT);
const LINK: Color32 = hex(theme::ACCENT_HOVER);
/// Corners of buttons and text fields.
const CONTROL_RADIUS: u8 = 6;
/// Corners of what holds them: rows, cards, menus.
const BOX_RADIUS: u8 = 8;
/// Around the text in text fields: as tall as a button.
const FIELD_MARGIN: Vec2 = Vec2::new(8.0, 6.0);
/// How long a toast stays, and one with an action (撤回, 接入).
const TOAST_LIFE: f32 = 4.5;
const TOAST_ACTION_LIFE: f32 = 8.0;
const TOAST_IN: f32 = 0.28;
const TOAST_OUT: f32 = 0.22;
/// The toasts at most on screen; a newer one pushes out the oldest.
const TOASTS_MAX: usize = 3;

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

enum ToastAction {
    /// Takes back the pending deletion with this id.
    Undo(u64),
    /// Uses a connection code found on the clipboard.
    Accept(String),
}

struct Toast {
    id: u64,
    tone: Tone,
    text: String,
    action: Option<(&'static str, ToastAction)>,
    life: f32,
    /// Seconds shown without the pointer on it.
    shown: f32,
    born: Instant,
    closing: Option<Instant>,
    /// The action button was used (a deletion is not carried out then).
    acted: bool,
}

/// What the status cards showed last, so they can fold away with their text.
#[derive(Default)]
struct Cards {
    trouble: String,
    busy: String,
    hosting: String,
    asking: String,
}

pub struct App {
    b: Backend,
    page: Page,
    paste: String,
    copied: Option<Instant>,
    /// The clipboard text looked at last (when the window got the focus).
    clip_seen: Option<String>,
    focused: bool,
    toasts: Vec<Toast>,
    pending: Vec<(u64, Undoable)>,
    next_id: u64,
    last_frame: Instant,
    /// The node's notice already shown.
    seen_notice: Option<String>,
    cards: Cards,
    /// Whether the service runs an older exe than this window, and when that was looked at.
    service_old: Option<(Instant, bool)>,
    /// 「以后」 on the card saying so, until the window closes.
    update_later: bool,
    renaming: Option<(EndpointId, String)>,
    /// The unreachable device whose explanation is open.
    why: Option<EndpointId>,
    /// The name in the settings while it is edited.
    name: String,
    saved: Option<Instant>,
    adding_relay: bool,
    relay_paste: String,
}

pub fn run(b: Backend) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("remote control")
            .with_inner_size([920.0, 600.0])
            .with_min_inner_size([720.0, 460.0])
            // An empty icon: eframe would otherwise put its own (or one picture
            // rescaled for every size) on the window on the first frame, over
            // the exe's drawn sizes that `theme::set_icon` gives it.
            .with_icon(egui::IconData::default()),
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
            }
            let ctx = cc.egui_ctx.clone();
            b.on_change(move || ctx.request_repaint());
            Ok(Box::new(App {
                b,
                page: Page::Devices,
                paste: String::new(),
                copied: None,
                clip_seen: None,
                focused: false,
                toasts: Vec::new(),
                pending: Vec::new(),
                next_id: 1,
                last_frame: Instant::now(),
                seen_notice: None,
                cards: Cards::default(),
                renaming: None,
                why: None,
                name: String::new(),
                saved: None,
                adding_relay: false,
                relay_paste: String::new(),
                service_old: None,
                update_later: false,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("lobby window: {e}"))
}

/// egui draws its own text and has no CJK glyphs: Microsoft YaHei from the
/// system goes first, egui's fonts stay as the fallback for symbols.
fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\msyh.ttf", r"C:\Windows\Fonts\simsun.ttc"] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("cjk".into(), Arc::new(FontData::from_owned(bytes)));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "cjk".into());
            fonts.families.entry(FontFamily::Monospace).or_default().push("cjk".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
    // Dark whatever the system uses, and winit keeps the frame's caption
    // buttons dark too when the system theme changes.
    ctx.set_theme(egui::Theme::Dark);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANE;
    v.window_fill = RAISED;
    v.window_stroke = Stroke::new(1.0, veil(20));
    v.window_corner_radius = CornerRadius::same(BOX_RADIUS);
    v.menu_corner_radius = CornerRadius::same(BOX_RADIUS);
    v.window_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(70) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(50) };
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
    // Separators.
    w.noninteractive.bg_stroke = Stroke::new(1.0, veil(18));
    w.noninteractive.corner_radius = CornerRadius::same(CONTROL_RADIUS);
    // Buttons use `weak_bg_fill`; check boxes `bg_fill`, a little stronger.
    for (state, fill, stroke) in
        [(&mut w.inactive, 16, 14), (&mut w.hovered, 28, 26), (&mut w.active, 10, 18), (&mut w.open, 28, 26)]
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
        s.spacing.menu_margin = Margin::same(6);
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(15.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(15.0));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(20.0));
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

/// A button whose accessible name says what it acts on (the visible text
/// alone would repeat on every row).
fn named_button(ui: &mut egui::Ui, b: impl egui::Widget, enabled: bool, name: String) -> egui::Response {
    let r = ui.add_enabled(enabled, b);
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, name.clone()));
    r
}

/// The main action: filled with the accent, lighter under the pointer,
/// darker while pressed.
struct Accent<'a>(egui::Button<'a>);

impl Accent<'_> {
    fn min_size(self, size: Vec2) -> Self {
        Accent(self.0.min_size(size))
    }
}

impl egui::Widget for Accent<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        // The state from the last frame, the way egui's buttons get theirs.
        let fill = match ui.ctx().read_response(ui.next_auto_id()) {
            Some(r) if r.enabled() && r.is_pointer_button_down_on() => hex(theme::ACCENT_PRESSED),
            Some(r) if r.enabled() && r.hovered() => hex(theme::ACCENT_HOVER),
            _ => ACCENT,
        };
        ui.add(self.0.fill(fill).stroke(Stroke::NONE))
    }
}

fn accent_button(text: &str) -> Accent<'_> {
    // The growing atoms on both sides centre the text in a wide button.
    let label = RichText::new(text).color(Color32::WHITE).strong();
    Accent(egui::Button::new((Atom::grow(), label, Atom::grow())).gap(0.0))
}

fn danger_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(DANGER))
}

fn ago(s: u64) -> String {
    match s {
        0..60 => "刚才".into(),
        60..3600 => format!("{} 分钟前", s / 60),
        _ => format!("{} 小时前", s / 3600),
    }
}

fn status_text(d: &Dev) -> (String, Color32) {
    match d.status {
        Status::Online { rtt_ms, path } => {
            let way = match path {
                PathKind::Direct => " · 直连",
                PathKind::Relay => " · 自建中转",
                PathKind::Public => " · 正在找直连",
                PathKind::None => "",
            };
            (format!("在线 {:.0} ms{way}", rtt_ms.max(1.0)), GREEN)
        }
        Status::Checking => ("正在找…".into(), DIM),
        Status::Offline => ("离线".into(), DIM),
        Status::Unreachable => ("打不通".into(), AMBER),
        Status::Refused(r) => (meet::refusal_text(r).into(), AMBER),
    }
}

fn tone_color(t: Tone) -> Color32 {
    match t {
        Tone::Ok => GREEN,
        Tone::Info => ACCENT,
        Tone::Warn => AMBER,
        Tone::Err => RED,
    }
}

/// Rows of text and buttons in `ui` start as tall as a button, so the text
/// sits centred next to the buttons instead of at their top.
fn button_lines(ui: &mut egui::Ui) {
    ui.spacing_mut().interact_size.y =
        ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y;
}

/// egui's spinner, at its usual size (it would grow with `button_lines`).
fn spinner(ui: &mut egui::Ui) {
    ui.add(egui::Spinner::new().size(18.0));
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 4.5, color);
}

#[derive(Clone, Copy)]
enum Icon {
    Trash,
    Copy,
    Back,
    Plus,
    Gear,
    Devices,
    Close,
    More,
}

/// Draws `icon` in `rect`, as lines on a 16 × 16 grid.
fn paint_icon(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width() / 16.0;
    let at = |x: f32, y: f32| rect.min + vec2(x * s, y * s);
    let st = Stroke::new(1.5, color);
    let line = |pts: &[(f32, f32)]| {
        p.line(pts.iter().map(|&(x, y)| at(x, y)).collect(), st);
    };
    match icon {
        Icon::Trash => {
            line(&[(2.5, 4.0), (13.5, 4.0)]);
            line(&[(6.0, 4.0), (6.0, 2.6), (10.0, 2.6), (10.0, 4.0)]);
            line(&[(4.0, 4.0), (4.7, 13.4), (11.3, 13.4), (12.0, 4.0)]);
            line(&[(6.6, 6.6), (6.6, 11.0)]);
            line(&[(9.4, 6.6), (9.4, 11.0)]);
        }
        Icon::Copy => {
            p.rect_stroke(Rect::from_min_max(at(5.5, 5.5), at(13.5, 13.5)), 1.5 * s, st, StrokeKind::Middle);
            line(&[(3.0, 10.5), (3.0, 2.5), (10.5, 2.5)]);
        }
        Icon::Back => line(&[(10.0, 3.0), (5.0, 8.0), (10.0, 13.0)]),
        Icon::Plus => {
            line(&[(8.0, 3.0), (8.0, 13.0)]);
            line(&[(3.0, 8.0), (13.0, 8.0)]);
        }
        Icon::Gear => {
            p.circle_stroke(at(8.0, 8.0), 2.3 * s, st);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let (c, sn) = (a.cos(), a.sin());
                line(&[(8.0 + 4.4 * c, 8.0 + 4.4 * sn), (8.0 + 6.2 * c, 8.0 + 6.2 * sn)]);
            }
        }
        Icon::Devices => {
            p.rect_stroke(Rect::from_min_max(at(1.8, 2.8), at(14.2, 10.8)), 1.2 * s, st, StrokeKind::Middle);
            line(&[(8.0, 10.8), (8.0, 13.5)]);
            line(&[(5.0, 13.5), (11.0, 13.5)]);
        }
        Icon::Close => {
            line(&[(4.0, 4.0), (12.0, 12.0)]);
            line(&[(12.0, 4.0), (4.0, 12.0)]);
        }
        Icon::More => {
            for x in [3.5, 8.0, 12.5] {
                p.circle_filled(at(x, 8.0), 1.3 * s, color);
            }
        }
    }
}

/// A square button with an icon: no fill until the pointer is on it. `red`:
/// it deletes something, and turns red under the pointer.
fn icon_button(ui: &mut egui::Ui, icon: Icon, size: f32, name: &str, red: bool) -> egui::Response {
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
    paint_icon(ui.painter(), Rect::from_center_size(rect.center(), Vec2::splat(16.0)), icon, color);
    r.on_hover_text(name)
}

/// An on/off switch, the knob sliding over.
fn switch(ui: &mut egui::Ui, on: &mut bool, enabled: bool, name: &str) -> egui::Response {
    let (rect, mut r) = ui.allocate_exact_size(vec2(38.0, 22.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    r.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, *on, name));
    let t = ui.ctx().animate_bool_with_time_and_easing(r.id, *on, 0.18, easing::cubic_out);
    let fade = if enabled { 1.0 } else { 0.4 };
    let off_ring = if r.hovered() && enabled { veil(110) } else { veil(70) };
    let fill = veil(10).lerp_to_gamma(ACCENT, t).gamma_multiply(fade);
    let ring = off_ring.lerp_to_gamma(ACCENT, t).gamma_multiply(fade);
    ui.painter().rect(rect, 11.0, fill, Stroke::new(1.0, ring), StrokeKind::Inside);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    let knob = hex(0xc9cbd0).lerp_to_gamma(Color32::WHITE, t).gamma_multiply(fade);
    ui.painter().circle_filled(pos2(x, rect.center().y), 7.0, knob);
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

/// A status card: a coloured dot, what is going on, its buttons.
fn card(ui: &mut egui::Ui, color: Color32, busy: bool, text: &str, buttons: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(ROW)
        .stroke(Stroke::new(1.0, veil(12)))
        .corner_radius(BOX_RADIUS)
        .inner_margin(Margin { left: 14, right: 10, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            button_lines(ui);
            ui.horizontal(|ui| {
                if busy {
                    spinner(ui);
                } else {
                    dot(ui, color);
                }
                ui.label(text);
                ui.with_layout(Layout::right_to_left(Align::Center), buttons);
            });
        });
}

/// One line of a settings group: a title, a line under it, a control on the
/// right, all centred on the line.
struct Item<'a> {
    title: &'a str,
    title_font: FontId,
    desc: Option<(&'a str, Color32)>,
    desc_font: FontId,
    /// A dot before the title, in this colour.
    lead: Option<Color32>,
    /// The group's first line: no divider above.
    first: bool,
    enabled: bool,
}

impl<'a> Item<'a> {
    fn new(title: &'a str) -> Self {
        Item { title, title_font: body(), desc: None, desc_font: small(), lead: None, first: false, enabled: true }
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
        const PAD: Vec2 = Vec2::new(14.0, 11.0);
        /// Kept for the controls on the right.
        const RIGHT: f32 = 240.0;
        let w = ui.available_width();
        let indent = if self.lead.is_some() { 18.0 } else { 0.0 };
        let text_w = (w - 2.0 * PAD.x - RIGHT - indent).max(160.0);
        let fade = if self.enabled { 1.0 } else { 0.5 };
        let title = ui.painter().layout(self.title.to_string(), self.title_font, TEXT.gamma_multiply(fade), text_w);
        let desc = self.desc.map(|(d, c)| ui.painter().layout(d.to_string(), self.desc_font, c.gamma_multiply(fade), text_w));
        let text_h = title.size().y + desc.as_ref().map_or(0.0, |g| g.size().y + 1.0);
        let h = (text_h + 2.0 * PAD.y).max(52.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        let p = ui.painter();
        if !self.first {
            p.hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(10)));
        }
        let x = rect.left() + PAD.x;
        let y = rect.center().y - text_h / 2.0;
        if let Some(c) = self.lead {
            p.circle_filled(pos2(x + 4.0, y + title.size().y / 2.0), 4.0, c.gamma_multiply(fade));
        }
        p.galley(pos2(x + indent, y), title.clone(), TEXT);
        if let Some(g) = desc {
            p.galley(pos2(x + indent, y + title.size().y + 1.0), g, DIM);
        }
        let right_rect = Rect::from_min_max(pos2(rect.right() - PAD.x - RIGHT, rect.top()), pos2(rect.right() - PAD.x, rect.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(right_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
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

/// A group of settings lines on one card.
fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).size(13.0).color(DIM));
    Frame::new().fill(ROW).corner_radius(BOX_RADIUS).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
    });
}

fn body() -> FontId {
    FontId::proportional(15.0)
}

fn small() -> FontId {
    FontId::proportional(13.0)
}

/// A wide button with an icon in the side pane; `on`: the page shown.
fn nav(ui: &mut egui::Ui, icon: Icon, text: &str, on: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));
    let fill = if on {
        veil(16)
    } else if r.hovered() {
        veil(8)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CONTROL_RADIUS, fill);
    let icon_rect = Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(16.0));
    paint_icon(ui.painter(), icon_rect, icon, if on { TEXT } else { DIM });
    ui.painter().text(pos2(rect.left() + 38.0, rect.center().y), egui::Align2::LEFT_CENTER, text, body(), TEXT);
    r
}

/// Width of a popup menu.
const MENU_WIDTH: f32 = 168.0;

/// One line of a popup menu: clickable across its width, lit while the
/// pointer is on it (red-tinted for `danger`).
fn menu_item(ui: &mut egui::Ui, text: &str, danger: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));
    let t = ui.ctx().animate_bool_with_time(r.id, ui.rect_contains_pointer(rect), 0.1);
    if t > 0.0 {
        let fill = if danger { DANGER.gamma_multiply(0.14 * t) } else { veil(14).gamma_multiply(t) };
        ui.painter().rect_filled(rect, CONTROL_RADIUS, fill);
    }
    let color = if danger { DANGER } else { TEXT };
    ui.painter().text(pos2(rect.left() + 10.0, rect.center().y), egui::Align2::LEFT_CENTER, text, body(), color);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
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
        let snap = self.b.snap();
        self.take_notices(&snap);
        self.watch_clipboard(&ctx, &snap);
        self.paste_anywhere(&ctx);
        if self.page == Page::Settings && ctx.memory(|m| m.focused().is_none()) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.page = Page::Devices;
        }
        egui::Panel::left("me")
            .resizable(false)
            .show_separator_line(false)
            .exact_size(300.0)
            .frame(Frame::new().fill(PANE).inner_margin(20.0))
            .show(ui, |ui| self.left(ui, &snap));
        // The content area lies on the pane like a sheet: round where it
        // meets the pane and the title bar, with a faint edge there. It runs
        // past the window's right and bottom edges, so the edge shows only
        // on the left and at the top; the margins keep 20 points inside.
        let layer = Frame::new()
            .fill(BG)
            .corner_radius(CornerRadius { nw: BOX_RADIUS, ne: 0, sw: 0, se: 0 })
            .stroke(Stroke::new(1.0, veil(12)))
            .outer_margin(Margin { left: 0, right: -2, top: 0, bottom: -2 })
            .inner_margin(Margin { left: 19, right: 9, top: 1, bottom: 0 });
        let content = egui::CentralPanel::default()
            .frame(layer)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Room for the scroll bar on the right, and above and below the rows
                    // (inside the scrolled part, so rows scroll up to the edge).
                    ui.set_width(ui.available_width() - 12.0);
                    ui.add_space(18.0);
                    match self.page {
                        Page::Devices => self.devices(ui, &snap),
                        Page::Settings => self.settings(ui, &snap),
                    }
                    ui.add_space(21.0);
                });
            })
            .response
            .rect;
        self.toasts(&ctx, content);
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl App {
    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        self.push_toast(tone, text.into(), None);
    }

    fn push_toast(&mut self, tone: Tone, text: String, action: Option<(&'static str, ToastAction)>) {
        let open = self.toasts.iter().filter(|t| t.closing.is_none()).count();
        if open >= TOASTS_MAX
            && let Some(old) = self.toasts.iter_mut().find(|t| t.closing.is_none())
        {
            old.closing = Some(Instant::now());
        }
        let id = self.new_id();
        let life = if action.is_some() { TOAST_ACTION_LIFE } else { TOAST_LIFE };
        self.toasts.push(Toast { id, tone, text, action, life, shown: 0.0, born: Instant::now(), closing: None, acted: false });
    }

    /// Hides `what` at once and carries it out when its toast is gone,
    /// unless 撤回 was clicked.
    fn undoable(&mut self, what: Undoable, text: String) {
        let id = self.new_id();
        self.pending.push((id, what));
        self.push_toast(Tone::Info, text, Some(("撤回", ToastAction::Undo(id))));
    }

    fn carry_out(&self, what: Undoable) {
        match what {
            Undoable::Revoke(token) => self.b.act(&format!("revoke\t{token}")),
            Undoable::Relay(url) => self.b.act(&format!("relay-remove\t{url}")),
        }
    }

    /// Deleting a device takes effect at once, on both sides: waiting for an
    /// undo would leave the other side showing it for as long.
    fn delete(&mut self, d: &Dev) {
        if self.renaming.as_ref().is_some_and(|(id, _)| *id == d.id) {
            self.renaming = None;
        }
        self.b.act(&format!("remove\t{}", d.id));
        let there = if matches!(d.status, Status::Online { .. } | Status::Unreachable) {
            "对方那边也删掉了本机"
        } else {
            "对方上线时也会删掉本机"
        };
        self.toast(Tone::Info, format!("已删除「{}」，{there}", d.name));
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
            self.toast(Tone::Warn, text.clone());
        }
        for n in self.b.take_notices() {
            self.toast(n.tone, n.text);
        }
    }

    /// When the window gets the focus, a connection code on the clipboard is
    /// offered (`target-plan` 4.2), unless it is this computer's own.
    fn watch_clipboard(&mut self, ctx: &egui::Context, snap: &Snap) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        let gained = focused && !self.focused;
        self.focused = focused;
        if !gained || snap.me.is_empty() {
            return;
        }
        let Some(text) = clipboard_text() else { return };
        if self.clip_seen.as_deref() == Some(text.as_str()) {
            return;
        }
        if let Some((code, _)) = ConnCode::find(&text)
            && code.device.to_string() != snap.me
            && !snap.devs.iter().any(|d| d.id == code.device && matches!(d.status, Status::Online { .. }))
        {
            self.push_toast(Tone::Info, "剪贴板里有对方的连接码".into(), Some(("接入", ToastAction::Accept(code.encode()))));
        }
        self.clip_seen = Some(text);
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
        ui.label(RichText::new("本机").color(DIM));
        ui.label(RichText::new(if snap.name.is_empty() { "…" } else { &snap.name }).size(24.0).strong());
        ui.add_space(14.0);
        let size = Vec2::new(260.0, 40.0);
        let done = self.copied.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        let copy = if done {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
            let label = RichText::new("✓ 已复制").color(Color32::WHITE).strong();
            ui.add(egui::Button::new((Atom::grow(), label, Atom::grow())).gap(0.0).fill(DONE).stroke(Stroke::NONE).min_size(size))
        } else {
            ui.add_enabled(!snap.me.is_empty(), accent_button("复制连接码").min_size(size))
        };
        if copy.clicked() {
            match self.b.request("code") {
                Ok(code) => {
                    ui.ctx().copy_text(code.clone());
                    self.clip_seen = Some(code);
                    self.copied = Some(Instant::now());
                }
                Err(e) => self.toast(Tone::Err, format!("{e:#}")),
            }
        }
        ui.add_space(4.0);
        let edit = ui.add(
            TextEdit::singleline(&mut self.paste).hint_text("粘贴对方的连接码").desired_width(260.0).margin(FIELD_MARGIN),
        );
        if edit.changed() && ConnCode::find(&self.paste).is_some() {
            let t = self.paste.clone();
            self.submit_code(&t);
        }
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if enter && !self.paste.trim().is_empty() {
            let t = self.paste.clone();
            self.submit_code(&t);
        }
        let codes: Vec<_> = snap.codes.iter().filter(|c| !self.hidden_code(&c.token)).cloned().collect();
        if !codes.is_empty() {
            ui.add_space(14.0);
            ui.label(RichText::new("未用的码").size(13.0).color(DIM));
            for c in codes {
                button_lines(ui);
                ui.horizontal(|ui| {
                    ui.set_width(260.0);
                    ui.label(ago(c.age_s));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let r = named_button(ui, egui::Button::new("撤销"), true, format!("撤销 {}", ago(c.age_s)));
                        if r.clicked() {
                            self.undoable(Undoable::Revoke(c.token.clone()), "已撤销连接码".into());
                        }
                    });
                });
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if nav(ui, Icon::Gear, "设置", self.page == Page::Settings).clicked() {
                self.page = Page::Settings;
            }
            if nav(ui, Icon::Devices, "设备", self.page == Page::Devices).clicked() {
                self.page = Page::Devices;
            }
            ui.add_space(6.0);
            ui.separator();
        });
    }

    /// What is going on now, above the device list; each card folds away
    /// when it no longer holds.
    fn status_cards(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        let trouble = self.b.trouble();
        if let Some(t) = &trouble {
            self.cards.trouble = t.clone();
        }
        let text = self.cards.trouble.clone();
        reveal(ui, Id::new("card-trouble"), trouble.is_some(), 10.0, |ui| card(ui, DIM, true, &text, |_| {}));

        let busy = self.b.busy();
        if let Some(t) = &busy {
            self.cards.busy = t.clone();
        }
        let text = self.cards.busy.clone();
        reveal(ui, Id::new("card-busy"), busy.is_some(), 10.0, |ui| card(ui, ACCENT, true, &text, |_| {}));

        if let Some(n) = &snap.hosting {
            self.cards.hosting = format!("「{n}」正在控制本机");
        }
        let text = self.cards.hosting.clone();
        let b = self.b.clone();
        reveal(ui, Id::new("card-hosting"), snap.hosting.is_some(), 10.0, |ui| {
            card(ui, RED, false, &text, |ui| {
                if ui.add(danger_button("断开")).clicked() {
                    b.act("disconnect");
                }
            })
        });

        if let Some(n) = &snap.asking {
            self.cards.asking = format!("「{n}」请求控制本机");
        }
        let text = self.cards.asking.clone();
        reveal(ui, Id::new("card-asking"), snap.asking.is_some(), 10.0, |ui| card(ui, AMBER, false, &text, |_| {}));

        let suggest = snap.suggest.is_some() && snap.hosting.is_none();
        let idle = self.b.busy().is_none();
        reveal(ui, Id::new("card-suggest"), suggest, 10.0, |ui| {
            card(ui, ACCENT, false, "装上后台服务，锁屏时也能被控制", |ui| {
                if ui.button("以后").clicked() {
                    b.act("unsuggest");
                }
                if ui.add_enabled(idle, accent_button("安装")).clicked() {
                    b.act("unsuggest");
                    b.service(true);
                }
            })
        });

        let old = self.service_old() && !self.update_later;
        let mut later = false;
        reveal(ui, Id::new("card-update"), old, 10.0, |ui| {
            card(ui, ACCENT, false, "后台服务还是旧版本，更新后才用上这一版", |ui| {
                later = ui.button("以后").clicked();
                if ui.add_enabled(idle, accent_button("更新")).clicked() {
                    b.service(true);
                }
            })
        });
        self.update_later |= later;
    }

    /// `install::service_outdated`, looked at every few seconds.
    fn service_old(&mut self) -> bool {
        if self.service_old.is_none_or(|(t, _)| t.elapsed() >= Duration::from_secs(5)) {
            let old = self.b.service_installed() && crate::install::service_outdated();
            self.service_old = Some((Instant::now(), old));
        }
        self.service_old.is_some_and(|(_, old)| old)
    }

    fn devices(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        self.status_cards(ui, snap);
        let sessions = self.b.sessions();
        ui.heading("我的设备");
        let mine: Vec<&Dev> = snap.devs.iter().filter(|d| d.listed).collect();
        if mine.is_empty() {
            ui.label(RichText::new("暂无").color(DIM));
        }
        for d in mine {
            self.row(ui, d, sessions.get(&d.id).map(|s| s.1));
        }
        ui.add_space(22.0);
        ui.heading("附近和临时");
        let temp: Vec<&Dev> = snap.devs.iter().filter(|d| !d.listed).collect();
        if temp.is_empty() {
            ui.label(RichText::new("暂无").color(DIM));
        }
        for d in temp {
            self.row(ui, d, sessions.get(&d.id).map(|s| s.1));
        }
    }

    fn row(&mut self, ui: &mut egui::Ui, d: &Dev, session: Option<SessionState>) {
        let (status, color) = status_text(d);
        let online = matches!(d.status, Status::Online { .. });
        let unreachable = d.status == Status::Unreachable;
        let row_id = Id::new(("row", d.id));
        // The row's own click (double-click: control) comes first, so the
        // buttons on it stay on top; its place is the one of the last frame.
        let last: Option<Rect> = ui.data(|m| m.get_temp(row_id));
        let row_resp = last.map(|r| ui.interact(r, row_id, Sense::click()));
        let hovered = row_resp.as_ref().is_some_and(|r| r.hovered());
        if online && session.is_none() && row_resp.as_ref().is_some_and(|r| r.double_clicked()) {
            self.b.control(d.id, &d.name);
        }
        let frame = Frame::new()
            .fill(if hovered { ROW_HOVER } else { ROW })
            .corner_radius(CornerRadius::same(BOX_RADIUS))
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                button_lines(ui);
                ui.horizontal(|ui| {
                    dot(ui, if online { GREEN } else if unreachable { AMBER } else { DIM });
                    let renaming = self.renaming.as_ref().is_some_and(|(id, _)| *id == d.id);
                    if renaming {
                        let (_, text) = self.renaming.as_mut().unwrap();
                        let e = ui.add(TextEdit::singleline(text).desired_width(200.0).margin(FIELD_MARGIN));
                        if !e.has_focus() && !e.lost_focus() {
                            e.request_focus();
                        }
                        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                        if esc {
                            self.renaming = None;
                        } else if e.lost_focus() {
                            let (id, text) = self.renaming.take().unwrap();
                            if !text.trim().is_empty() && text.trim() != d.name {
                                self.b.act(&format!("rename\t{id}\t{}", text.replace(['\t', '\n'], " ")));
                            }
                        }
                    } else {
                        let name = RichText::new(&d.name).size(16.0).color(if online || !d.listed { TEXT } else { DIM });
                        let r = ui.add(egui::Label::new(name).sense(Sense::click())).on_hover_text(d.id.to_string());
                        if d.listed && r.double_clicked() {
                            self.renaming = Some((d.id, d.name.clone()));
                        }
                    }
                    let tag = if !d.listed {
                        if d.met == "code" { "连接码" } else { "局域网" }
                    } else if d.auto {
                        "我的电脑"
                    } else {
                        "信任设备"
                    };
                    ui.label(RichText::new(tag).color(DIM).size(13.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if d.listed {
                            self.row_menu(ui, d);
                        } else if d.met == "code" && icon_button(ui, Icon::Trash, 30.0, &format!("删除 {}", d.name), true).clicked() {
                            self.delete(d);
                        }
                        match session {
                            Some(s) => {
                                let t = match s {
                                    SessionState::Connecting => "正在连接…",
                                    SessionState::Waiting => "等对方确认…",
                                    SessionState::Connected => "控制中",
                                    SessionState::Paused => "画面暂停",
                                };
                                if s != SessionState::Connected && s != SessionState::Paused {
                                    spinner(ui);
                                }
                                ui.label(RichText::new(t).color(ACCENT));
                            }
                            None if unreachable => {
                                let r = named_button(ui, egui::Button::new("为什么"), true, format!("为什么 {}", d.name));
                                if r.clicked() {
                                    self.why = if self.why == Some(d.id) { None } else { Some(d.id) };
                                }
                            }
                            None => {
                                let name = format!("控制 {}", d.name);
                                let r = if online {
                                    named_button(ui, accent_button("控制"), true, name)
                                } else {
                                    named_button(ui, egui::Button::new("控制"), false, name)
                                };
                                if r.clicked() {
                                    self.b.control(d.id, &d.name);
                                }
                            }
                        }
                        ui.label(RichText::new(status).size(14.0).color(color));
                    });
                });
                let open = unreachable && self.why == Some(d.id);
                let b = self.b.clone();
                let mut to_settings = false;
                reveal(ui, Id::new(("why", d.id)), open, 0.0, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("找不到直连。打开路由器的 IPv6，或者加一台自建中转。").color(DIM));
                            ui.horizontal(|ui| {
                                if ui.button("重试").clicked() {
                                    b.act(&format!("retry\t{}", d.id));
                                }
                                if ui.button("去设置").clicked() {
                                    to_settings = true;
                                }
                            });
                        });
                    });
                });
                if to_settings {
                    self.page = Page::Settings;
                }
            });
        // Where the row's click goes on the next frame.
        let rect = frame.response.rect;
        if last != Some(rect) {
            ui.data_mut(|m| m.insert_temp(row_id, rect));
            ui.ctx().request_repaint();
        }
        ui.add_space(6.0);
    }

    fn row_menu(&mut self, ui: &mut egui::Ui, d: &Dev) {
        let more = icon_button(ui, Icon::More, 30.0, &format!("更多 {}", d.name), false);
        egui::Popup::menu(&more).align(RectAlign::BOTTOM_END).gap(4.0).show(|ui| {
            ui.set_width(MENU_WIDTH);
            ui.spacing_mut().item_spacing.y = 2.0;
            if menu_item(ui, "终端", false).clicked() {
                ui.close();
                if let Err(e) = crate::term::open_window(d.id, &d.name) {
                    self.toast(Tone::Err, format!("打不开终端：{e:#}"));
                }
            }
            if menu_item(ui, "重命名", false).clicked() {
                self.renaming = Some((d.id, d.name.clone()));
                ui.close();
            }
            let row = menu_item(ui, "免确认", false);
            let at = Rect::from_center_size(pos2(row.rect.right() - 29.0, row.rect.center().y), vec2(38.0, 22.0));
            let mut auto = d.auto;
            let flip = ui.put(at, |ui: &mut egui::Ui| switch(ui, &mut auto, true, "免确认")).changed();
            if flip || row.clicked() {
                self.b.act(&format!("auto\t{}\t{}", d.id, !d.auto as u8));
            }
            ui.separator();
            if menu_item(ui, "删除", true).clicked() {
                ui.close();
                self.delete(d);
            }
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui, snap: &Snap) {
        ui.horizontal(|ui| {
            ui.add_space(-6.0);
            if icon_button(ui, Icon::Back, 32.0, "返回", false).clicked() {
                self.page = Page::Devices;
            }
            ui.heading("设置");
        });
        ui.add_space(6.0);
        let b = self.b.clone();

        group(ui, "本机", |ui| {
            let name = &mut self.name;
            let saved = &mut self.saved;
            Item::new("名字").desc("对方看到的名字").first(true).show(ui, |ui| {
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
                        ui.label(RichText::new("✓ 已保存").size(13.0).color(GREEN.gamma_multiply(a)));
                    }
                }
            });
            let mut copied = false;
            Item { desc_font: FontId::monospace(12.0), ..Item::new("设备码").desc(&snap.me) }.show(ui, |ui| {
                copied = icon_button(ui, Icon::Copy, 32.0, "复制设备码", false).clicked();
            });
            if copied {
                ui.ctx().copy_text(snap.me.clone());
                self.toast(Tone::Ok, "设备码已复制");
            }
        });

        ui.add_space(14.0);
        group(ui, "连接", |ui| {
            let mut public = snap.public;
            Item::new("公共节点牵线").desc("只交换地址，不传画面").first(true).show(ui, |ui| {
                if switch(ui, &mut public, true, "公共节点牵线").changed() {
                    b.act(&format!("set\tpublic\t{}", public as u8));
                }
            });
            let mut lan = snap.lan;
            Item::new("局域网可见").show(ui, |ui| {
                if switch(ui, &mut lan, true, "局域网可见").changed() {
                    b.act(&format!("set\tlan\t{}", lan as u8));
                }
            });
        });

        ui.add_space(14.0);
        self.relays(ui, snap);

        ui.add_space(14.0);
        group(ui, "被控制", |ui| {
            let mut accept = snap.accept_new;
            Item::new("接受新设备的请求").desc("关掉后只有我的设备能请求").first(true).show(ui, |ui| {
                if switch(ui, &mut accept, true, "接受新设备的请求").changed() {
                    b.act(&format!("set\taccept_new\t{}", accept as u8));
                }
            });
            let mut clip = snap.clipboard;
            Item::new("同步剪贴板").show(ui, |ui| {
                if switch(ui, &mut clip, true, "同步剪贴板").changed() {
                    b.act(&format!("set\tclipboard\t{}", clip as u8));
                }
            });
        });

        ui.add_space(14.0);
        let old = self.service_old();
        group(ui, "后台服务", |ui| {
            let installed = b.service_installed();
            let busy = b.busy().is_some();
            let (title, desc) = match (installed, snap.service) {
                (true, true) if old => ("运行中，旧版本", "更新后才用上这一版"),
                (true, true) => ("运行中", "关掉窗口也在线"),
                (true, false) => ("已安装，没在运行", "装上后锁屏时也能被控制"),
                _ => ("没装", "装上后锁屏时也能被控制"),
            };
            Item::new(title).desc(desc).first(true).show(ui, |ui| {
                if !installed {
                    if ui.add_enabled(!busy, accent_button("安装")).clicked() {
                        b.service(true);
                    }
                } else if ui.add_enabled(!busy, egui::Button::new("卸载")).clicked() {
                    b.service(false);
                }
                if installed && old && ui.add_enabled(!busy, accent_button("更新")).clicked() {
                    b.service(true);
                }
            });
            let on = snap.service;
            for (key, title, desc, value) in [
                ("boot", "开机自动在线", None, snap.boot),
                ("lock", "锁屏时可被控制", Some("仅限我的电脑"), snap.lock),
                ("awake", "插电时不睡眠", Some("屏幕照常熄灭"), snap.awake),
            ] {
                let mut v = value;
                let line = Item::new(title).enabled(on);
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

        ui.add_space(14.0);
        let mut taught = None;
        group(ui, "AI", |ui| {
            Item::new("让本机的 AI 会用")
                .desc("Claude Code、Codex 学会用终端操作设备列表里的电脑")
                .first(true)
                .show(ui, |ui| {
                    if ui.button("写入").clicked() {
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
    /// A relay code pasted under them adds one; each added here can go
    /// (with 撤回). One given by another computer (this one is "my
    /// computer" there) says so, and goes when that computer stops giving it.
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
        ui.add_space(4.0);
        ui.label(RichText::new("自建中转").size(13.0).color(DIM));
        Frame::new().fill(ROW).corner_radius(BOX_RADIUS).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, r) in relays.iter().enumerate() {
                let (state, color) = match r.state.as_str() {
                    "home" => ("在用", GREEN),
                    "temp" => ("临时在用（首选连不上）", GREEN),
                    "fail" => ("连不上", AMBER),
                    "idle" => ("未使用，名单照常同步", DIM),
                    _ => ("正在连接…", DIM),
                };
                let host = host_of(&r.url);
                let (mut remove, mut pick) = (false, false);
                let line = Item {
                    title_font: FontId::monospace(13.5),
                    desc: Some((state, if color == AMBER { AMBER } else { DIM })),
                    lead: Some(color),
                    ..Item::new(&host).first(i == 0)
                };
                line.show(ui, |ui| {
                    remove = icon_button(ui, Icon::Trash, 32.0, &format!("移除 {host}"), true).clicked();
                    if i != 0 {
                        pick = ui.button("使用").on_hover_text("以后优先用这台中转").clicked();
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
            if relays.is_empty() && !self.adding_relay {
                Item { title_font: small(), ..Item::new("暂无").first(true) }.show(ui, |_| {});
            }
            let first = relays.is_empty() && !self.adding_relay;
            if self.adding_relay {
                if !relays.is_empty() {
                    let r = ui.available_rect_before_wrap();
                    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, veil(10)));
                }
                Frame::new().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                    button_lines(ui);
                    ui.horizontal(|ui| {
                        let w = ui.available_width() - 150.0;
                        let e = ui.add_enabled(
                            !running,
                            TextEdit::singleline(&mut self.relay_paste).hint_text("粘贴中转码").desired_width(w).margin(FIELD_MARGIN),
                        );
                        if ui.ctx().memory(|m| m.focused().is_none()) && !running && self.relay_paste.is_empty() {
                            e.request_focus();
                        }
                        let enter = e.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let esc = e.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape));
                        let ready = !running && !self.relay_paste.trim().is_empty();
                        if ui.button("取消").clicked() || esc {
                            self.adding_relay = false;
                            self.relay_paste.clear();
                        }
                        let add = if running {
                            ui.add_enabled(false, egui::Button::new("正在添加…"))
                        } else {
                            ui.add_enabled(ready, accent_button("添加"))
                        };
                        if (add.clicked() || enter) && ready {
                            self.b.start_job(format!("relay-claim\t{}", self.relay_paste.replace(['\t', '\r', '\n'], " ")));
                        }
                    });
                });
            } else {
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
                r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "添加中转"));
                if !first || !relays.is_empty() {
                    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, veil(10)));
                }
                if r.hovered() {
                    ui.painter().rect_filled(rect, CornerRadius { nw: 0, ne: 0, sw: BOX_RADIUS, se: BOX_RADIUS }, veil(6));
                }
                let icon = Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(16.0));
                paint_icon(ui.painter(), icon, Icon::Plus, LINK);
                ui.painter().text(pos2(rect.left() + 40.0, rect.center().y), egui::Align2::LEFT_CENTER, "添加中转", body(), LINK);
                if r.clicked() {
                    self.adding_relay = true;
                }
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new("打不通时画面走第一台中转；它连不上时，下一台能连上的临时顶替").size(13.0).color(DIM));
    }

    /// The toasts, centred at the top of the page: they slide in, stay a
    /// few seconds (longer with an action, the clock stopped while the
    /// pointer is on one), and slide out; the ones below move up.
    fn toasts(&mut self, ctx: &egui::Context, area: Rect) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint();
        let width = (area.width() - 40.0).min(460.0);
        let x = area.center().x - width / 2.0;
        let mut y = area.top() + 12.0;
        let mut acted: Vec<(u64, ToastAction)> = Vec::new();
        for t in &mut self.toasts {
            let enter = easing::cubic_out((t.born.elapsed().as_secs_f32() / TOAST_IN).min(1.0));
            let exit = t.closing.map_or(0.0, |c| easing::cubic_in((c.elapsed().as_secs_f32() / TOAST_OUT).min(1.0)));
            let ty = ctx.animate_value_with_time(Id::new(("toast-y", t.id)), y, 0.22);
            let pos = pos2(x, ty - 14.0 * (1.0 - enter) - 10.0 * exit);
            let alpha = enter * (1.0 - exit);
            let mut close = false;
            let mut action_clicked = false;
            let remaining = (1.0 - t.shown / t.life).clamp(0.0, 1.0);
            let r = egui::Area::new(Id::new(("toast", t.id))).order(Order::Foreground).fixed_pos(pos).constrain(false).show(ctx, |ui| {
                ui.set_opacity(alpha);
                let inner = Frame::new()
                    .fill(RAISED)
                    .stroke(Stroke::new(1.0, veil(18)))
                    .corner_radius(10)
                    .shadow(Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) })
                    .inner_margin(Margin { left: 14, right: 6, top: 6, bottom: 6 })
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        button_lines(ui);
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                close = icon_button(ui, Icon::Close, 28.0, "关闭", false).clicked();
                                if let Some((label, _)) = &t.action {
                                    let b = egui::Button::new(RichText::new(*label).color(LINK).strong()).frame(false);
                                    action_clicked = ui.add(b).clicked();
                                    ui.add_space(4.0);
                                }
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    dot(ui, tone_color(t.tone));
                                    ui.add(egui::Label::new(RichText::new(&t.text).size(14.0)).wrap());
                                });
                            });
                        });
                    });
                // How long it still stays.
                let rect = inner.response.rect.shrink2(vec2(10.0, 0.0));
                let bar = Rect::from_min_size(pos2(rect.left(), rect.bottom() - 2.0), vec2(rect.width() * remaining, 2.0));
                ui.painter().rect_filled(bar, 1.0, veil(28));
            });
            let hovered = r.response.contains_pointer();
            if t.closing.is_none() {
                if !hovered {
                    t.shown += dt;
                }
                if action_clicked {
                    t.acted = true;
                    if let Some((_, a)) = t.action.take() {
                        acted.push((t.id, a));
                    }
                }
                if close || action_clicked || t.shown >= t.life {
                    t.closing = Some(now);
                }
            }
            y += (r.response.rect.height() + 8.0) * (1.0 - exit);
        }
        for (_, a) in acted {
            match a {
                ToastAction::Undo(id) => self.pending.retain(|(p, _)| *p != id),
                ToastAction::Accept(code) => self.submit_code(&code),
            }
        }
        // Gone: a deletion without 撤回 is carried out now.
        let mut gone = Vec::new();
        self.toasts.retain_mut(|t| {
            let done = t.closing.is_some_and(|c| c.elapsed().as_secs_f32() >= TOAST_OUT);
            if done && !t.acted
                && let Some((_, ToastAction::Undo(id))) = t.action.take()
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
