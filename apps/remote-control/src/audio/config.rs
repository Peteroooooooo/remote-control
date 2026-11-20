//! Per-device audio settings kept in `%APPDATA%\remote-control\audio.txt`:
//! `<device_id>\t<enabled 0|1>\t<mono|stereo>\t<mute_host 0|1>[\t<opus|lossless>]`.
//! Non-default entries only.

use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use tokio::sync::mpsc::UnboundedSender;

use crate::{
    log,
    proto::{AudioCfg, Msg},
};

struct State {
    device: String,
    choice: AudioCfg,
    ctrl: Option<UnboundedSender<Msg>>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static VIEWER_ENABLED: AtomicBool = AtomicBool::new(true);
static ACTIVE_RATE_HZ: AtomicU32 = AtomicU32::new(super::codec::SAMPLE_RATE);

fn format_line(device: &str, cfg: &AudioCfg) -> String {
    let ch = if cfg.channels == 1 { "mono" } else { "stereo" };
    let mode = if cfg.lossless { "lossless" } else { "opus" };
    format!("{device}\t{}\t{ch}\t{}\t{mode}", cfg.enabled as u8, cfg.mute_host as u8)
}

fn parse_line(line: &str) -> Option<(&str, AudioCfg)> {
    let f: Vec<&str> = line.trim_end().split('\t').collect();
    let (device, en, ch, mute, mode) = match f[..] {
        [device, en, ch, mute] => (device, en, ch, mute, "opus"),
        [device, en, ch, mute, mode] => (device, en, ch, mute, mode),
        _ => return None,
    };
    if device.is_empty() {
        return None;
    }
    let enabled = match en {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let channels = match ch {
        "mono" => 1,
        "stereo" => 2,
        _ => return None,
    };
    let mute_host = match mute {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let lossless = match mode {
        "opus" => false,
        "lossless" => true,
        _ => return None,
    };
    Some((device, AudioCfg { enabled, channels, mute_host, lossless, max_rate_hz: 0 }))
}

fn path() -> PathBuf {
    crate::net::default_key_path().with_file_name("audio.txt")
}

fn load(device: &str) -> Option<AudioCfg> {
    let text = std::fs::read_to_string(path()).ok()?;
    text.lines().filter_map(parse_line).find(|(d, _)| *d == device).map(|(_, c)| c)
}

fn store(device: &str, choice: &AudioCfg) -> std::io::Result<()> {
    let path = path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: String = text
        .lines()
        .filter(|l| parse_line(l).is_some_and(|(d, _)| d != device))
        .map(|l| format!("{l}\n"))
        .collect();
    let persisted = AudioCfg { max_rate_hz: 0, ..*choice };
    if persisted != AudioCfg::default() {
        out += &format!("{}\n", format_line(device, &persisted));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &path)
}

pub fn start(device: &str) -> AudioCfg {
    let mut choice = load(device).unwrap_or_default();
    choice.max_rate_hz = super::wasapi::local_render_rate();
    VIEWER_ENABLED.store(choice.enabled, Ordering::Relaxed);
    log!("audio: {choice:?}");
    *STATE.lock().unwrap() = Some(State { device: device.to_string(), choice, ctrl: None });
    choice
}

pub fn connect(ctrl: UnboundedSender<Msg>) {
    if let Some(s) = STATE.lock().unwrap().as_mut() {
        s.ctrl = Some(ctrl);
    }
}

pub fn choice() -> AudioCfg {
    STATE.lock().unwrap().as_ref().map(|s| s.choice).unwrap_or_default()
}

pub fn choose(f: impl FnOnce(&mut AudioCfg)) {
    let changed = {
        let mut g = STATE.lock().unwrap();
        let Some(s) = g.as_mut() else { return };
        let before = s.choice;
        f(&mut s.choice);
        s.choice.channels = s.choice.channels.clamp(1, 2);
        if s.choice == before {
            false
        } else {
            VIEWER_ENABLED.store(s.choice.enabled, Ordering::Relaxed);
            log!("audio: {:?}", s.choice);
            if let Err(e) = store(&s.device, &s.choice) {
                log!("audio choice not saved: {e}");
            }
            if let Some(c) = &s.ctrl {
                let _ = c.send(Msg::Audio(s.choice));
            }
            true
        }
    };
    if changed {
        crate::toolbar::refresh();
    }
}

pub fn toggle_enabled() {
    choose(|c| c.enabled = !c.enabled);
}

pub fn viewer_enabled() -> bool {
    VIEWER_ENABLED.load(Ordering::Relaxed)
}

pub fn active_rate_hz() -> u32 {
    ACTIVE_RATE_HZ.load(Ordering::Relaxed)
}

pub fn set_active_rate_hz(rate: u32) {
    let rate = rate.clamp(44_100, 192_000);
    if ACTIVE_RATE_HZ.swap(rate, Ordering::Relaxed) != rate {
        crate::toolbar::refresh();
    }
}

/// Updates the viewer's local output device sample rate and informs the host so
/// lossless mode uses `min(host_rate, viewer_rate)`.
pub fn set_viewer_rate_hz(rate: u32) {
    let rate = if rate == 0 { 0 } else { super::codec::normalize_lossless_rate(rate) };
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    if s.choice.max_rate_hz == rate {
        return;
    }
    s.choice.max_rate_hz = rate;
    log!("audio: viewer native rate {rate} Hz");
    if let Some(c) = &s.ctrl {
        let _ = c.send(Msg::Audio(s.choice));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        for c in [
            AudioCfg::default(),
            AudioCfg { enabled: false, channels: 1, mute_host: true, lossless: false, max_rate_hz: 0 },
            AudioCfg { enabled: true, channels: 1, mute_host: false, lossless: true, max_rate_hz: 0 },
            AudioCfg { enabled: true, channels: 2, mute_host: true, lossless: true, max_rate_hz: 0 },
        ] {
            let line = format_line("dev-123", &c);
            assert_eq!(parse_line(&line), Some(("dev-123", c)), "{line}");
        }
        // Legacy 4-column format defaults to Opus (lossless = false).
        assert_eq!(
            parse_line("dev-old\t1\tstereo\t1"),
            Some(("dev-old", AudioCfg { enabled: true, channels: 2, mute_host: true, lossless: false, max_rate_hz: 0 }))
        );
        assert_eq!(parse_line("\t1\tstereo\t0"), None);
        assert_eq!(parse_line("dev\t2\tstereo\t0"), None);
        assert_eq!(parse_line("dev\t1\tquad\t0"), None);
        assert_eq!(parse_line("dev\t1\tmono\tx"), None);
        assert_eq!(parse_line("dev\t1\tmono\t0\tflac"), None);
        assert_eq!(parse_line("dev\t1\tmono"), None);
    }
}
