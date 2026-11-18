//! The management screen: `rc-relay` with no command, in the container's
//! console. Left, the relay's codes; right, the devices that joined with the
//! selected one; below, the selected code in full, to copy with the mouse.
//! Keys: ↑↓ select, Tab / ←→ switch lists, n new code, r rename, d delete,
//! q quit. Changes go straight into the state files, which the running
//! relay picks up within 2 s; who is online comes from its `status.txt`.

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use iroh_base::{EndpointId, RelayUrl};
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use rc_ident::{proof::unix_now, relay_code::RelayCode};

use crate::{
    config::Config,
    state::{Code, Owner, Status, Store},
};

const ACCENT: Color = Color::Rgb(91, 140, 255);
const CYAN: Color = Color::Rgb(95, 215, 215);
const GREEN: Color = Color::Rgb(74, 222, 128);
const RED: Color = Color::Rgb(248, 113, 113);
const FG: Color = Color::Rgb(215, 218, 224);
const DIM: Color = Color::Rgb(107, 114, 128);
const LINE: Color = Color::Rgb(52, 58, 74);
const SEL: Color = Color::Rgb(30, 42, 74);
const SEL_IDLE: Color = Color::Rgb(26, 31, 43);
const KEY_BG: Color = Color::Rgb(40, 46, 62);
const TOAST_BG: Color = Color::Rgb(19, 38, 27);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Codes,
    Devices,
}

enum Modal {
    Rename(String),
    DeleteCode,
    DeleteDevice,
}

struct App {
    store: Store,
    urls: Vec<RelayUrl>,
    codes: Vec<Code>,
    owners: Vec<Owner>,
    status: Status,
    sel: usize,
    dsel: usize,
    focus: Focus,
    modal: Option<Modal>,
    toast: Option<(String, Instant, bool)>,
    /// The demo's temporary directory, removed on quit.
    demo: Option<PathBuf>,
}

pub fn run(cfg: &Config, demo: bool) -> Result<()> {
    let (store, urls, demo_dir) = if demo {
        let dir = std::env::temp_dir().join(format!("rc-relay-demo-{}", std::process::id()));
        let store = make_demo(&dir)?;
        (store, demo_urls(), Some(dir))
    } else {
        let store = Store::new(&cfg.state_dir);
        store.upgrade()?;
        let urls = cfg.saved_urls().context("先启动一次中转（rc-relay serve，容器默认就会），再打开管理界面")?;
        store.first_code()?;
        (store, urls, None)
    };
    let mut app = App {
        store,
        urls,
        codes: Vec::new(),
        owners: Vec::new(),
        status: Status::default(),
        sel: 0,
        dsel: 0,
        focus: Focus::Codes,
        modal: None,
        toast: None,
        demo: demo_dir,
    };
    app.reload()?;
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("管理界面需要一个终端（docker exec -it rc-relay rc-relay）；只看文字用 rc-relay codes");
    }
    let mut term = ratatui::try_init().context("打开全屏界面")?;
    let r = app.main_loop(&mut term);
    ratatui::restore();
    if let Some(d) = &app.demo {
        let _ = std::fs::remove_dir_all(d);
    }
    r
}

impl App {
    fn reload(&mut self) -> Result<()> {
        self.codes = self.store.codes()?;
        self.owners = self.store.owners()?;
        self.status = self.store.status().unwrap_or_default();
        self.sel = self.sel.min(self.codes.len().saturating_sub(1));
        let n = self.devices().len();
        self.dsel = self.dsel.min(n.saturating_sub(1));
        if n == 0 {
            self.focus = Focus::Codes;
        }
        Ok(())
    }

    fn code(&self) -> Option<&Code> {
        self.codes.get(self.sel)
    }

    fn devices(&self) -> Vec<&Owner> {
        let Some(c) = self.code() else { return Vec::new() };
        self.owners.iter().filter(|o| o.code == c.n).collect()
    }

    fn count(&self, c: &Code) -> usize {
        self.owners.iter().filter(|o| o.code == c.n).count()
    }

    /// Whether the relay wrote its status lately (it writes every minute).
    fn running(&self) -> bool {
        self.demo.is_some() || unix_now().saturating_sub(self.status.written) < 90
    }

    fn online(&self) -> HashSet<EndpointId> {
        if self.running() { self.status.online.iter().copied().collect() } else { HashSet::new() }
    }

    fn toast(&mut self, text: impl Into<String>, ok: bool) {
        self.toast = Some((text.into(), Instant::now(), ok));
    }

    fn main_loop(&mut self, term: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut last_read = Instant::now();
        loop {
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(500))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press && !self.key(k.code, k.modifiers)? {
                        return Ok(());
                    }
                }
            }
            if self.toast.as_ref().is_some_and(|t| t.1.elapsed() > Duration::from_secs(3)) {
                self.toast = None;
            }
            // Devices joining and going on- and offline, without a key press.
            if last_read.elapsed() > Duration::from_secs(2) && self.modal.is_none() {
                self.reload()?;
                last_read = Instant::now();
            }
        }
    }

    /// False: quit.
    fn key(&mut self, key: KeyCode, mods: KeyModifiers) -> Result<bool> {
        if mods.contains(KeyModifiers::CONTROL) && key == KeyCode::Char('c') {
            return Ok(false);
        }
        match self.modal.take() {
            Some(Modal::Rename(mut text)) => {
                match key {
                    KeyCode::Enter => {
                        let c = self.code().cloned().expect("a code is selected");
                        self.store.rename_code(c.n, text.trim())?;
                        self.reload()?;
                        let now = self.code().map(Code::shown_name).unwrap_or_default();
                        self.toast(format!("已改名为「{now}」"), true);
                    }
                    KeyCode::Esc => {}
                    KeyCode::Backspace => {
                        text.pop();
                        self.modal = Some(Modal::Rename(text));
                    }
                    KeyCode::Char(ch) => {
                        if text.chars().count() < 24 {
                            text.push(ch);
                        }
                        self.modal = Some(Modal::Rename(text));
                    }
                    _ => self.modal = Some(Modal::Rename(text)),
                }
                return Ok(true);
            }
            Some(m @ (Modal::DeleteCode | Modal::DeleteDevice)) => {
                if matches!(key, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                    if matches!(m, Modal::DeleteCode) {
                        let c = self.code().cloned().expect("a code is selected");
                        let gone = self.store.delete_code(c.n)?;
                        self.reload()?;
                        self.toast(
                            if gone.is_empty() { format!("已删除「{}」", c.shown_name()) } else { format!("已删除「{}」，{} 台电脑已移出", c.shown_name(), gone.len()) },
                            true,
                        );
                    } else {
                        let o = self.devices()[self.dsel].clone();
                        self.store.remove_owner(&o.id)?;
                        self.reload()?;
                        self.toast(format!("已移除 {}", shown(&o)), true);
                    }
                }
                return Ok(true);
            }
            None => {}
        }
        let n = self.devices().len();
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::Codes if self.sel + 1 < self.codes.len() => {
                    self.sel += 1;
                    self.dsel = 0;
                }
                Focus::Devices if self.dsel + 1 < n => self.dsel += 1,
                _ => {}
            },
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::Codes if self.sel > 0 => {
                    self.sel -= 1;
                    self.dsel = 0;
                }
                Focus::Devices if self.dsel > 0 => self.dsel -= 1,
                _ => {}
            },
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Codes && n > 0 { Focus::Devices } else { Focus::Codes };
            }
            KeyCode::Right | KeyCode::Char('l') if n > 0 => self.focus = Focus::Devices,
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Codes,
            KeyCode::Char('n') => {
                let c = self.store.new_code("")?;
                self.reload()?;
                self.sel = self.codes.iter().position(|x| x.n == c.n).unwrap_or(0);
                self.focus = Focus::Codes;
                self.toast(format!("已生成「{}」，按 r 可以改名", c.shown_name()), true);
            }
            KeyCode::Char('r') if self.code().is_some() => {
                self.modal = Some(Modal::Rename(self.code().map(|c| c.name.clone()).unwrap_or_default()));
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if self.focus == Focus::Devices && n > 0 {
                    self.modal = Some(Modal::DeleteDevice);
                } else if self.code().is_some() {
                    self.modal = Some(Modal::DeleteCode);
                }
            }
            _ => {}
        }
        Ok(true)
    }

    fn draw(&self, f: &mut Frame) {
        let area = f.area().inner(Margin::new(1, 0));
        let [head, _, body, _, code, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_head(f, head);
        let [left, right] = Layout::horizontal([Constraint::Percentage(34), Constraint::Fill(1)]).spacing(1).areas(body);
        self.draw_codes(f, left);
        self.draw_devices(f, right);
        self.draw_code(f, code);
        self.draw_keys(f, keys);
        if let Some(m) = &self.modal {
            self.draw_modal(f, m);
        }
    }

    fn draw_head(&self, f: &mut Frame, area: Rect) {
        let hosts: Vec<String> = self
            .urls
            .iter()
            .map(|u| format!("{}{}", u.host_str().unwrap_or("?"), u.port().map(|p| format!(":{p}")).unwrap_or_default()))
            .collect();
        let state = if self.running() {
            let up = if self.status.started > 0 { format!(" · 已运行 {}", ago(unix_now().saturating_sub(self.status.started))) } else { String::new() };
            vec![Span::styled("● ", Style::new().fg(GREEN)), Span::styled(format!("运行中 · {}{up}", hosts.join(" · ")), Style::new().fg(DIM))]
        } else {
            vec![Span::styled("● ", Style::new().fg(RED)), Span::styled(format!("中转没有在运行 · {}", hosts.join(" · ")), Style::new().fg(DIM))]
        };
        let left = Line::from(vec![
            Span::styled("rc-relay", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(if self.demo.is_some() { "  自建中转 · 演示数据" } else { "  自建中转" }, Style::new().fg(DIM)),
        ]);
        let right = Line::from(state).right_aligned();
        let w = left.width() as u16 + 2;
        let [l, r] = Layout::horizontal([Constraint::Length(w), Constraint::Fill(1)]).areas(area);
        f.render_widget(left, l);
        f.render_widget(right, r);
    }

    fn panel<'a>(&self, title: String, focused: bool) -> Block<'a> {
        let c = if focused { ACCENT } else { LINE };
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(c))
            .title(Span::styled(format!(" {title} "), Style::new().fg(if focused { ACCENT } else { DIM })))
    }

    fn row_style(&self, selected: bool, focused: bool) -> Style {
        match (selected, focused) {
            (true, true) => Style::new().bg(SEL).fg(FG),
            (true, false) => Style::new().bg(SEL_IDLE).fg(FG),
            _ => Style::new().fg(FG),
        }
    }

    fn draw_codes(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Codes;
        let block = self.panel("中转码".into(), focused);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if self.codes.is_empty() {
            let p = Paragraph::new(vec![Line::from(""), Line::from(vec![Span::styled(" 没有中转码，按 ", Style::new().fg(DIM)), Span::styled("n", Style::new().fg(FG)), Span::styled(" 新建一个", Style::new().fg(DIM))])]);
            f.render_widget(p, inner);
            return;
        }
        let top = self.sel.saturating_sub(inner.height.saturating_sub(1) as usize);
        for (i, c) in self.codes.iter().enumerate().skip(top).take(inner.height as usize) {
            let y = inner.y + (i - top) as u16;
            let row = Rect::new(inner.x, y, inner.width, 1);
            let sel = i == self.sel;
            let style = self.row_style(sel, focused);
            f.render_widget(Block::new().style(style), row);
            let bar = if sel && focused { Span::styled("▌", Style::new().fg(ACCENT)) } else { Span::raw(" ") };
            let count = format!("{} 台 ", self.count(c));
            let name_w = (inner.width as usize).saturating_sub(count.len() + 3);
            let line = Line::from(vec![bar, Span::raw(" "), Span::raw(fit(&c.shown_name(), name_w))]).style(style);
            f.render_widget(line, row);
            f.render_widget(Line::from(Span::styled(count, Style::new().fg(if sel && focused { Color::Rgb(159, 180, 255) } else { DIM }))).right_aligned(), row);
        }
    }

    fn draw_devices(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Devices;
        let title = self.code().map_or_else(|| "电脑".to_string(), |c| format!("「{}」的电脑", c.shown_name()));
        let block = self.panel(title, focused);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(_) = self.code() else { return };
        let devs = self.devices();
        if devs.is_empty() {
            let p = Paragraph::new(vec![
                Line::from(""),
                Line::styled(" 还没有电脑用这个码加入。", Style::new().fg(DIM)),
                Line::from(vec![Span::styled(" 把下面的码贴到软件 ", Style::new().fg(DIM)), Span::styled("「设置 → 自建中转」", Style::new().fg(FG))]),
                Line::styled(" 那台电脑就会出现在这里。", Style::new().fg(DIM)),
            ])
            .wrap(Wrap { trim: false });
            f.render_widget(p, inner);
            return;
        }
        let online = self.online();
        let top = self.dsel.saturating_sub(inner.height.saturating_sub(1) as usize);
        for (i, o) in devs.iter().enumerate().skip(top).take(inner.height as usize) {
            let y = inner.y + (i - top) as u16;
            let row = Rect::new(inner.x, y, inner.width, 1);
            let sel = focused && i == self.dsel;
            let style = self.row_style(sel, focused);
            f.render_widget(Block::new().style(style), row);
            let on = online.contains(&o.id);
            let when = if on {
                "在线".to_string()
            } else {
                match self.status.seen.get(&o.id) {
                    Some(t) => format!("{}前", ago(unix_now().saturating_sub(*t))),
                    None => "离线".to_string(),
                }
            };
            let id = o.id.fmt_short().to_string();
            let right_w = 10 + 12;
            let name_w = (inner.width as usize).saturating_sub(right_w + 4);
            let bar = if sel { Span::styled("▌", Style::new().fg(ACCENT)) } else { Span::raw(" ") };
            let dot = if on { Span::styled("● ", Style::new().fg(GREEN)) } else { Span::styled("○ ", Style::new().fg(DIM)) };
            f.render_widget(Line::from(vec![bar, dot, Span::raw(fit(&shown(o), name_w))]).style(style), row);
            let tail = Line::from(vec![
                Span::styled(format!("{id:<12}"), Style::new().fg(DIM)),
                Span::styled(format!("{when:>8} "), Style::new().fg(if on { GREEN } else { DIM })),
            ])
            .right_aligned();
            f.render_widget(tail, row);
        }
    }

    fn draw_code(&self, f: &mut Frame, area: Rect) {
        let Some(c) = self.code() else {
            let block = self.panel("完整中转码".into(), false);
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(Line::styled(" 按 n 新建一个中转码", Style::new().fg(DIM)), inner);
            return;
        };
        let block = self.panel(format!("完整中转码 · 「{}」", c.shown_name()), false).padding(ratatui::widgets::Padding::horizontal(1));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let text = RelayCode { urls: self.urls.clone(), claim_key: c.key }.encode();
        let p = Paragraph::new(vec![
            Line::styled(text, Style::new().fg(CYAN)),
            Line::styled("鼠标拖选复制，贴到软件「设置 → 自建中转」。同一组的电脑贴同一个码。", Style::new().fg(DIM)),
        ])
        .wrap(Wrap { trim: false });
        f.render_widget(p, inner);
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let key = |k: &'static str, what: &'static str| {
            vec![Span::styled(format!(" {k} "), Style::new().fg(FG).bg(KEY_BG)), Span::styled(format!(" {what}   "), Style::new().fg(DIM))]
        };
        let line: Vec<Span> =
            [("↑↓", "选择"), ("Tab", "切换"), ("n", "新建码"), ("r", "改名"), ("d", "删除"), ("q", "退出")].into_iter().flat_map(|(k, w)| key(k, w)).collect();
        f.render_widget(Line::from(line), area);
        if let Some((t, _, ok)) = &self.toast {
            let text = format!(" {} {t} ", if *ok { "✓" } else { "!" });
            let w = Line::from(text.as_str()).width() as u16;
            let r = Rect::new(area.right().saturating_sub(w), area.y, w.min(area.width), 1);
            f.render_widget(Clear, r);
            f.render_widget(Line::styled(text, Style::new().fg(if *ok { GREEN } else { RED }).bg(TOAST_BG)), r);
        }
    }

    fn draw_modal(&self, f: &mut Frame, m: &Modal) {
        // Everything behind goes dim.
        let area = f.area();
        let buf = f.buffer_mut();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_style(Style::new().fg(Color::Rgb(58, 62, 72)).bg(Color::Reset));
                }
            }
        }
        let (title, color, body, keys): (String, Color, Vec<Line>, Vec<(&str, &str)>) = match m {
            Modal::Rename(text) => {
                let c = self.code().map(Code::shown_name).unwrap_or_default();
                (
                    format!("给「{c}」改名"),
                    ACCENT,
                    vec![
                        Line::styled("名字只给你自己看，方便区分是给谁的；留空就叫「码 N」", Style::new().fg(DIM)),
                        Line::from(""),
                        Line::from(vec![Span::styled(" ", Style::new()), Span::styled(text.clone(), Style::new().fg(FG)), Span::styled("▏", Style::new().fg(ACCENT))]),
                    ],
                    vec![("Enter", "保存"), ("Esc", "取消")],
                )
            }
            Modal::DeleteCode => {
                let c = self.code().expect("a code is selected");
                let n = self.count(c);
                (
                    format!("删除「{}」？", c.shown_name()),
                    RED,
                    vec![
                        Line::styled(if n > 0 { format!("这个码作废，下面 {n} 台电脑立即不能再用中转。") } else { "这个码作废。".to_string() }, Style::new().fg(FG)),
                        Line::styled("已经发出去的这个码也就失效了。", Style::new().fg(DIM)),
                    ],
                    vec![("y", "删除"), ("n", "取消")],
                )
            }
            Modal::DeleteDevice => {
                let o = self.devices()[self.dsel];
                let c = self.code().map(Code::shown_name).unwrap_or_default();
                (
                    format!("移除 {}？", shown(o)),
                    RED,
                    vec![
                        Line::styled("中转立即停止为它转发。", Style::new().fg(FG)),
                        Line::styled(format!("它手里还有「{c}」的码，可以再贴回来；想彻底踢掉，请删掉整个码。"), Style::new().fg(DIM)),
                    ],
                    vec![("y", "移除"), ("n", "取消")],
                )
            }
        };
        let w = 60.min(area.width.saturating_sub(4));
        let h = (body.len() as u16 + 6).min(area.height);
        let r = Rect::new(area.x + (area.width.saturating_sub(w)) / 2, area.y + (area.height.saturating_sub(h)) / 2, w, h);
        f.render_widget(Clear, r);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .title(Span::styled(format!(" {title} "), Style::new().fg(color).add_modifier(Modifier::BOLD)))
            .padding(ratatui::widgets::Padding::new(2, 2, 1, 0));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let mut lines = body;
        lines.push(Line::from(""));
        lines.push(Line::from(keys.into_iter().flat_map(|(k, w)| [Span::styled(format!(" {k} "), Style::new().fg(FG).bg(KEY_BG)), Span::styled(format!(" {w}   "), Style::new().fg(DIM))]).collect::<Vec<_>>()));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

fn shown(o: &Owner) -> String {
    if o.name.is_empty() { format!("设备 {}", o.id.fmt_short()) } else { o.name.clone() }
}

/// `text` cut to `w` columns (CJK characters take two).
fn fit(text: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = Span::raw(ch.to_string()).width();
        if used + cw > w {
            if used < w {
                out.push('…');
            }
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

/// A span of seconds as people say it.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "1 分钟".into(),
        60..3600 => format!("{} 分钟", secs / 60),
        3600..86_400 => format!("{} 小时", secs / 3600),
        _ => format!("{} 天", secs / 86_400),
    }
}

fn demo_urls() -> Vec<RelayUrl> {
    ["https://203.0.113.7:7843/?rc-cert=demo", "https://[2001:db8::7]:7843/?rc-cert=demo"].iter().map(|u| u.parse().expect("valid")).collect()
}

/// Made-up codes and devices to try the screen with.
fn make_demo(dir: &std::path::Path) -> Result<Store> {
    let _ = std::fs::remove_dir_all(dir);
    let store = Store::new(dir);
    let key = |n: u8| iroh_base::SecretKey::from_bytes(&[n; 32]).public();
    let mine = store.new_code("")?;
    let home = store.new_code("爸妈")?;
    store.new_code("")?;
    store.join(key(1), mine.n, "DESKTOP")?;
    store.join(key(2), mine.n, "laptop")?;
    store.join(key(3), home.n, "DAD-PC")?;
    let now = unix_now();
    store.set_status(&Status {
        started: now - 3 * 86_400,
        written: now,
        online: vec![key(1), key(3)],
        seen: [(key(2), now - 3 * 86_400)].into_iter().collect(),
    })?;
    Ok(store)
}
