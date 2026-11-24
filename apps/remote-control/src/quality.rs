//! The viewer's 画面 menu: resolution, frame rate, highest bitrate,
//! 文字清晰 (YUV 4:4:4, offered when both ends can) and 线路 (only through a
//! self-hosted relay, offered when there is one), kept per device in
//! `%APPDATA%\remote-control\quality.txt`, one line per device
//! (`<id>\t<resolution>\t<fps>\t<mbps>\t<sharp 0|1>\t<relay only 0|1>`, 0 for
//! 自动); a device without a line gets the defaults (自动 everywhere, off). A
//! picture choice goes to the host at once (`Msg::Quality`); the host answers
//! with what it streams now (`Msg::Info`'s `Streaming`), which the menu shows.
//! A 线路 choice makes the session window connect again (see `view`).
//!
//! 自动 resolution fits the screen the session window is on: the host never
//! sends more pixels than this screen can show, nor more than it has. 自动
//! frame rate is that screen's refresh rate (at most `AUTO_FPS_MAX`); the
//! host sends no more frames than its own screen shows.

use std::{path::PathBuf, sync::Mutex};

use tokio::sync::mpsc::UnboundedSender;
use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        Graphics::Gdi::{
            DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST,
            MONITORINFO, MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow,
        },
        UI::WindowsAndMessaging::GetCursorPos,
    },
    core::PCWSTR,
};

use crate::{
    log,
    proto::{Msg, Quality, Streaming},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Res {
    /// Fit this screen.
    Auto,
    /// The host's display as it is.
    Native,
    /// The shorter side at most this many pixels.
    Short(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub res: Res,
    /// 0 = 自动.
    pub fps: u8,
    /// 0 = 自动.
    pub mbps: u16,
    /// 文字清晰: 4:4:4 if both ends can.
    pub sharp: bool,
    /// 线路 只走中转: no direct path, only self-hosted relays.
    pub relay_only: bool,
}

impl Default for Choice {
    fn default() -> Self {
        Choice { res: Res::Auto, fps: 0, mbps: 0, sharp: false, relay_only: false }
    }
}

impl Res {
    pub fn label(self) -> &'static str {
        match self {
            Res::Auto => crate::i18n::tr("自动", "Auto"),
            Res::Native => crate::i18n::tr("原生", "Native"),
            Res::Short(1440) => "1440p",
            Res::Short(1080) => "1080p",
            Res::Short(720) => "720p",
            Res::Short(_) => "",
        }
    }
}

/// The menu's chips, in order (0 = Auto).
pub const RESOLUTIONS: [Res; 5] = [Res::Auto, Res::Native, Res::Short(1440), Res::Short(1080), Res::Short(720)];
pub const FRAME_RATES: [u8; 5] = [0, 30, 60, 120, 144];
pub const BITRATES: [u16; 6] = [0, 5, 10, 20, 40, 80];

/// 自动 frame rate on faster screens: the highest the menu offers.
const AUTO_FPS_MAX: u8 = 144;

/// The screen the session window is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    /// In pixels.
    pub size: (u16, u16),
    /// Refresh rate; 0 if Windows did not say.
    pub hz: u16,
}

struct State {
    device: String,
    choice: Choice,
    /// Kept in the file; not when the command line chose (tests).
    save: bool,
    screen: Screen,
    ctrl: Option<UnboundedSender<Msg>>,
    /// Where a 线路 choice goes (the session window's connection), and
    /// whether the menu offers it (a self-hosted relay to go through).
    line: Option<UnboundedSender<bool>>,
    /// What the host streams now: picture size and the rest. None from a
    /// host that does not know the menu (it keeps its own settings).
    streaming: Option<((u32, u32), Option<Streaming>)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

impl Choice {
    /// The frame rate to ask for on `screen`.
    pub fn fps_on(&self, screen: Screen) -> u8 {
        match self.fps {
            0 if screen.hz > 1 => screen.hz.min(AUTO_FPS_MAX as u16) as u8,
            0 => 60,
            f => f,
        }
    }

    /// What to ask the host for, on `screen`.
    pub fn quality(&self, screen: Screen) -> Quality {
        let (fit_w, fit_h) = if self.res == Res::Auto { screen.size } else { (0, 0) };
        let short = match self.res {
            Res::Short(s) => s,
            _ => 0,
        };
        Quality {
            fit_w,
            fit_h,
            short,
            fps: self.fps_on(screen),
            kbps: self.mbps as u32 * 1000,
            yuv444: self.sharp,
            auto_fps: self.fps == 0,
        }
    }

    fn line(&self, device: &str) -> String {
        let res = match self.res {
            Res::Auto => "auto".to_string(),
            Res::Native => "native".to_string(),
            Res::Short(s) => s.to_string(),
        };
        format!("{device}\t{res}\t{}\t{}\t{}\t{}", self.fps, self.mbps, self.sharp as u8, self.relay_only as u8)
    }

    fn parse(line: &str) -> Option<(&str, Choice)> {
        let f: Vec<&str> = line.trim_end().split('\t').collect();
        let (device, res, fps, mbps, sharp, relay_only) = match f[..] {
            [d, r, f, m] => (d, r, f, m, "0", "0"),
            [d, r, f, m, s] => (d, r, f, m, s, "0"),
            [d, r, f, m, s, o] => (d, r, f, m, s, o),
            _ => return None,
        };
        let res = match res {
            "auto" => Res::Auto,
            "native" => Res::Native,
            s => Res::Short(s.parse().ok().filter(|&v| v >= 240)?),
        };
        let flag = |s: &str| match s {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        Some((
            device,
            Choice { res, fps: fps.parse().ok()?, mbps: mbps.parse().ok()?, sharp: flag(sharp)?, relay_only: flag(relay_only)? },
        ))
    }
}

fn path() -> PathBuf {
    crate::net::default_key_path().with_file_name("quality.txt")
}

fn load(device: &str) -> Option<Choice> {
    let text = std::fs::read_to_string(path()).ok()?;
    text.lines().filter_map(Choice::parse).find(|(d, _)| *d == device).map(|(_, c)| c)
}

fn store(device: &str, choice: &Choice) -> std::io::Result<()> {
    let path = path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: String = text
        .lines()
        .filter(|l| Choice::parse(l).is_some_and(|(d, _)| d != device))
        .map(|l| format!("{l}\n"))
        .collect();
    if *choice != Choice::default() {
        out += &format!("{}\n", choice.line(device));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &path)
}

fn screen_of(m: HMONITOR) -> Screen {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if !unsafe { GetMonitorInfoW(m, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) }.as_bool() {
        return Screen { size: (0, 0), hz: 0 };
    }
    let r = mi.monitorInfo.rcMonitor;
    let size = ((r.right - r.left).clamp(0, 65535) as u16, (r.bottom - r.top).clamp(0, 65535) as u16);
    let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool();
    Screen { size, hz: if ok { dm.dmDisplayFrequency.min(1000) as u16 } else { 0 } }
}

/// The saved choice for `device` (before connecting: its 线路).
pub fn saved(device: &str) -> Choice {
    load(device).unwrap_or_default()
}

/// The session with `device` starts: its saved choice, or the command
/// line's frame rate and bitrate (not saved). Before the window exists the
/// screen is the one under the pointer. Returns what to ask for in the hello.
pub fn start(device: &str, fps: Option<u8>, mbps: Option<f64>) -> (Choice, Quality) {
    let mut choice = load(device).unwrap_or_default();
    let save = fps.is_none() && mbps.is_none();
    if let Some(f) = fps {
        choice.fps = f;
    }
    if let Some(m) = mbps {
        choice.mbps = m.clamp(1.0, 1000.0).round() as u16;
    }
    let mut p = POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    let screen = screen_of(unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST) });
    log!("picture: {choice:?} on a {}x{} screen at {} Hz", screen.size.0, screen.size.1, screen.hz);
    *STATE.lock().unwrap() =
        Some(State { device: device.to_string(), choice, save, screen, ctrl: None, line: None, streaming: None });
    (choice, choice.quality(screen))
}

/// Where to send later choices.
pub fn connect(ctrl: UnboundedSender<Msg>) {
    if let Some(s) = STATE.lock().unwrap().as_mut() {
        s.ctrl = Some(ctrl);
    }
}

/// The menu offers 线路; its choices go to `line`.
pub fn connect_line(line: UnboundedSender<bool>) {
    if let Some(s) = STATE.lock().unwrap().as_mut() {
        s.line = Some(line);
    }
}

pub fn line_offered() -> bool {
    STATE.lock().unwrap().as_ref().is_some_and(|s| s.line.is_some())
}

pub fn choice() -> Choice {
    STATE.lock().unwrap().as_ref().map(|s| s.choice).unwrap_or_default()
}

/// The screen the session window is on (the one under the pointer before it opens).
pub fn screen() -> Screen {
    STATE.lock().unwrap().as_ref().map_or(Screen { size: (0, 0), hz: 0 }, |s| s.screen)
}

/// The menu changed the choice: keep it and tell the host, or for 线路 the
/// connection.
pub fn choose(f: impl FnOnce(&mut Choice)) {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    let before = s.choice;
    f(&mut s.choice);
    if s.choice == before {
        return;
    }
    log!("picture: {:?}", s.choice);
    if s.save
        && let Err(e) = store(&s.device, &s.choice)
    {
        log!("picture choice not saved: {e}");
    }
    if s.choice.relay_only != before.relay_only {
        if let Some(l) = &s.line {
            let _ = l.send(s.choice.relay_only);
        }
    } else {
        send(s);
    }
}

/// The 线路 the connection uses, when not the one chosen: the relay did not
/// answer (saved, so the menu and the next session show what is in use), or
/// the command line's (`save` false, for tests).
pub fn line_used(relay_only: bool, save: bool) {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    if s.choice.relay_only == relay_only {
        return;
    }
    s.choice.relay_only = relay_only;
    if save
        && s.save
        && let Err(e) = store(&s.device, &s.choice)
    {
        log!("picture choice not saved: {e}");
    }
    drop(g);
    crate::toolbar::refresh();
}

fn send(s: &State) {
    if let Some(c) = &s.ctrl {
        let _ = c.send(Msg::Quality(s.choice.quality(s.screen)));
    }
}

/// The session window moved or changed size, or a display changed (its
/// thread): with 自动, another screen size or refresh rate asks the host for
/// another picture.
pub fn follow_window(hwnd: HWND) {
    let screen = screen_of(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) });
    if screen.size.0 == 0 {
        return;
    }
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    if s.screen == screen {
        return;
    }
    let before = s.choice.quality(s.screen);
    s.screen = screen;
    if s.choice.quality(screen) != before {
        log!("picture: now on a {}x{} screen at {} Hz", screen.size.0, screen.size.1, screen.hz);
        send(s);
    }
}

/// The host said what it streams now (`Msg::Info`).
pub fn set_streaming(size: (u32, u32), streaming: Option<Streaming>) {
    if let Some(s) = STATE.lock().unwrap().as_mut() {
        s.streaming = Some((size, streaming));
    }
    crate::toolbar::refresh();
}

/// For the menu's last line: what the host streams now, if it said.
pub fn streaming() -> Option<((u32, u32), Option<Streaming>)> {
    STATE.lock().unwrap().as_ref().and_then(|s| s.streaming)
}

/// Frames come no faster than the screen the session window is on shows
/// them: what the host said it streams, else what was asked for.
pub fn frames_fit_screen() -> bool {
    let g = STATE.lock().unwrap();
    let Some(s) = g.as_ref() else { return false };
    let fps = match s.streaming {
        Some((_, Some(st))) => st.fps,
        _ => s.choice.fps_on(s.screen),
    };
    s.screen.hz > 1 && u16::from(fps) <= s.screen.hz
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        for c in [
            Choice::default(),
            Choice { res: Res::Native, fps: 144, mbps: 80, sharp: true, relay_only: false },
            Choice { res: Res::Short(1080), fps: 30, mbps: 5, sharp: false, relay_only: true },
        ] {
            let line = c.line("abc");
            assert_eq!(Choice::parse(&line), Some(("abc", c)), "{line}");
        }
        assert_eq!(Choice::parse("abc\t100\t60\t0"), None, "no such resolution");
        assert_eq!(Choice::parse("abc\tauto\t60"), None);
        let auto = Choice::default();
        assert_eq!(Choice::parse("abc\tauto\t0\t0"), Some(("abc", auto)));
        let before_sharp = Choice { res: Res::Short(720), fps: 30, mbps: 5, ..auto };
        assert_eq!(Choice::parse("abc\t720\t30\t5"), Some(("abc", before_sharp)));
        let before_line = Choice { sharp: true, ..before_sharp };
        assert_eq!(Choice::parse("abc\t720\t30\t5\t1"), Some(("abc", before_line)));
        assert_eq!(Choice::parse("abc\t720\t30\t5\t1\t2"), None);
    }

    #[test]
    fn what_the_host_is_asked_for() {
        let screen = Screen { size: (2560, 1440), hz: 165 };
        let q = Choice::default().quality(screen);
        assert_eq!((q.fit_w, q.fit_h, q.short, q.fps, q.kbps, q.yuv444, q.auto_fps), (2560, 1440, 0, 144, 0, false, true));
        let q = Choice { res: Res::Short(720), fps: 30, mbps: 10, sharp: true, relay_only: false }.quality(screen);
        assert_eq!((q.fit_w, q.fit_h, q.short, q.fps, q.kbps, q.yuv444, q.auto_fps), (0, 0, 720, 30, 10_000, true, false));
        let q = Choice { res: Res::Native, fps: 120, ..Choice::default() }.quality(screen);
        assert_eq!((q.fit_w, q.fit_h, q.short, q.auto_fps), (0, 0, 0, false));
    }

    #[test]
    fn auto_frame_rate_follows_the_screen() {
        let auto = Choice::default();
        assert_eq!(auto.fps_on(Screen { size: (2880, 1920), hz: 120 }), 120);
        assert_eq!(auto.fps_on(Screen { size: (1920, 1080), hz: 59 }), 59);
        assert_eq!(auto.fps_on(Screen { size: (1920, 1080), hz: 240 }), 144, "at most the menu's highest");
        assert_eq!(auto.fps_on(Screen { size: (1920, 1080), hz: 0 }), 60, "unknown");
        assert_eq!(auto.fps_on(Screen { size: (1920, 1080), hz: 1 }), 60, "the hardware's default");
        let fixed = Choice { fps: 30, ..auto };
        assert_eq!(fixed.fps_on(Screen { size: (1920, 1080), hz: 120 }), 30);
    }
}
