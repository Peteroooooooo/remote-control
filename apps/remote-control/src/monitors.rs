//! The host's displays: listing them, opening the one the viewer picked, and
//! noticing when they change (plugged in or out, new resolution or rotation).
//! Displays are numbered left to right, then top to bottom, by their place on
//! the virtual desktop, so "next display" on the viewer moves to the right.
//! The choice is kept by device name, so it survives the list changing; when
//! the chosen display disappears the primary one is streamed instead.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::Dxgi::{
            Common::{
                DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
                DXGI_MODE_ROTATION_ROTATE270,
            },
            CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1,
        },
    },
    core::Interface,
};

use crate::{
    gpu::Gpu,
    log,
    proto::{MonitorDesc, Msg},
};

/// How often the capture loop looks for added, removed or changed displays.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// What the viewer picked and what is streamed now; shared by the capture
/// loop and the control stream.
#[derive(Default)]
pub struct Selection(Mutex<State>);

#[derive(Default)]
struct State {
    /// Device name the viewer asked for; None streams the primary display.
    wanted: Option<String>,
    /// The displays when the capture was last opened, and the streamed one.
    list: Vec<MonitorDesc>,
    current: usize,
    /// The capture loop should reopen on the wanted display.
    switch: bool,
    checked: Option<Instant>,
}

struct Found {
    desc: MonitorDesc,
    adapter: u32,
    output: u32,
}

pub fn rotation_degrees(r: DXGI_MODE_ROTATION) -> u16 {
    match r {
        DXGI_MODE_ROTATION_ROTATE90 => 90,
        DXGI_MODE_ROTATION_ROTATE180 => 180,
        DXGI_MODE_ROTATION_ROTATE270 => 270,
        _ => 0,
    }
}

/// Displays attached to the desktop, left to right.
fn enumerate() -> Result<Vec<Found>> {
    let mut found = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                let d = output.GetDesc()?;
                let r = d.DesktopCoordinates;
                let len = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
                let name = String::from_utf16_lossy(&d.DeviceName[..len]);
                if d.AttachedToDesktop.as_bool() && !found.iter().any(|f: &Found| f.desc.name == name) {
                    let desc = MonitorDesc {
                        name,
                        x: r.left,
                        y: r.top,
                        width: (r.right - r.left).max(0) as u32,
                        height: (r.bottom - r.top).max(0) as u32,
                        primary: r.left == 0 && r.top == 0,
                        rotation: rotation_degrees(d.Rotation),
                    };
                    found.push(Found { desc, adapter: i, output: j });
                }
                j += 1;
            }
            i += 1;
        }
    }
    found.sort_by_key(|f| (f.desc.x, f.desc.y));
    Ok(found)
}

/// The display to stream: the wanted one if it is there, else the primary
/// (else the first). The flag is true when the wanted one is gone.
fn choose(list: &[MonitorDesc], wanted: Option<&str>) -> Option<(usize, bool)> {
    if let Some(i) = wanted.and_then(|w| list.iter().position(|m| m.name == w)) {
        return Some((i, false));
    }
    let i = list.iter().position(|m| m.primary).unwrap_or(0);
    (!list.is_empty()).then_some((i, wanted.is_some()))
}

/// Opens the display to stream, on the adapter that drives it, with its
/// place on the desktop. Called by the capture loop before each (re)start.
/// The error when the desktop has no display at all (see `PipeShared::no_display`).
pub const NO_DISPLAY: &str = "no display attached to the desktop";

pub fn open(sel: &Selection) -> Result<(Gpu, IDXGIOutput1, RECT)> {
    let found = enumerate()?;
    let list: Vec<MonitorDesc> = found.iter().map(|f| f.desc.clone()).collect();
    let mut s = sel.0.lock().unwrap();
    let Some((i, gone)) = choose(&list, s.wanted.as_deref()) else { bail!("{NO_DISPLAY}") };
    if gone {
        log!("display {} is gone; streaming {} instead", s.wanted.take().unwrap_or_default(), list[i].name);
    }
    if s.list != list {
        for (n, m) in list.iter().enumerate() {
            log!(
                "display {}/{}: {} {}x{} at ({}, {}){}{}",
                n + 1,
                list.len(),
                m.name,
                m.width,
                m.height,
                m.x,
                m.y,
                if m.primary { ", primary" } else { "" },
                if m.rotation != 0 { format!(", rotated {}°", m.rotation) } else { String::new() },
            );
        }
    }
    let m = &list[i];
    let rect = RECT { left: m.x, top: m.y, right: m.x + m.width as i32, bottom: m.y + m.height as i32 };
    log!("streaming display {}/{} ({})", i + 1, list.len(), m.name);
    s.list = list;
    s.current = i;
    s.switch = false;
    s.checked = Some(Instant::now());
    drop(s);
    let f = &found[i];
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = factory.EnumAdapters1(f.adapter).context("display adapter went away")?;
        let output = adapter.EnumOutputs(f.output).context("display went away")?;
        Ok((Gpu::new(adapter)?, output.cast()?, rect))
    }
}

/// The list for the viewer, with the streamed display.
pub fn message(sel: &Selection) -> Msg {
    let s = sel.0.lock().unwrap();
    Msg::Monitors { list: s.list.clone(), current: s.current as u8 }
}

/// The streamed display's refresh rate in Hz: the most frames it can show
/// (60 when Windows does not say).
pub fn refresh_hz(sel: &Selection) -> u32 {
    use windows::{
        Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW},
        core::HSTRING,
    };
    let name = {
        let s = sel.0.lock().unwrap();
        s.list.get(s.current).map(|m| m.name.clone())
    };
    let Some(name) = name else { return 60 };
    let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(&HSTRING::from(name), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool();
    // 0 and 1 mean "the hardware's default".
    if ok && dm.dmDisplayFrequency > 1 { dm.dmDisplayFrequency } else { 60 }
}

/// The viewer asked for display `index` (0-based) of the list it was sent.
/// True when the capture has to move to another display.
pub fn select(sel: &Selection, index: u8) -> Result<bool> {
    let mut s = sel.0.lock().unwrap();
    let i = index as usize;
    if i >= s.list.len() {
        bail!("the viewer asked for display {}, but there are {}", i + 1, s.list.len());
    }
    if i == s.current {
        return Ok(false);
    }
    s.wanted = Some(s.list[i].name.clone());
    s.switch = true;
    Ok(true)
}

/// Called often by the capture loop: true when it should reopen, because the
/// viewer picked another display or the displays changed.
pub fn should_reopen(sel: &Selection) -> bool {
    {
        let mut s = sel.0.lock().unwrap();
        if s.switch {
            return true;
        }
        if s.checked.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
            return false;
        }
        s.checked = Some(Instant::now());
    }
    let Ok(found) = enumerate() else { return false };
    let s = sel.0.lock().unwrap();
    let changed = found.len() != s.list.len() || found.iter().zip(&s.list).any(|(f, m)| f.desc != *m);
    if changed {
        log!("displays changed ({} now)", found.len());
    }
    changed
}

/// Index of the display to ask for: `step` -1 / +1 cycles from `current`
/// through `count` displays, wrapping around.
pub fn cycle(current: usize, count: usize, step: i32) -> usize {
    if count == 0 {
        return 0;
    }
    (current as i64 + step as i64).rem_euclid(count as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: i32, primary: bool) -> MonitorDesc {
        MonitorDesc { name: name.into(), x, y: 0, width: 1920, height: 1080, primary, rotation: 0 }
    }

    #[test]
    fn choose_wanted_then_primary() {
        let list = [mon("B", -1920, false), mon("A", 0, true), mon("C", 1920, false)];
        assert_eq!(choose(&list, None), Some((1, false)));
        assert_eq!(choose(&list, Some("C")), Some((2, false)));
        assert_eq!(choose(&list, Some("gone")), Some((1, true)), "falls back to the primary");
        let no_primary = [mon("B", -1920, false), mon("C", 1920, false)];
        assert_eq!(choose(&no_primary, None), Some((0, false)));
        assert_eq!(choose(&[], Some("A")), None);
    }

    #[test]
    fn select_checks_the_index() {
        let sel = Selection::default();
        sel.0.lock().unwrap().list = vec![mon("A", 0, true), mon("B", 1920, false)];
        assert!(!select(&sel, 0).unwrap(), "already streamed");
        assert!(select(&sel, 2).is_err());
        assert!(!sel.0.lock().unwrap().switch);
        assert!(select(&sel, 1).unwrap());
        assert!(should_reopen(&sel));
        assert_eq!(sel.0.lock().unwrap().wanted.as_deref(), Some("B"));
    }

    #[test]
    fn cycle_wraps() {
        assert_eq!(cycle(0, 3, 1), 1);
        assert_eq!(cycle(2, 3, 1), 0);
        assert_eq!(cycle(0, 3, -1), 2);
        assert_eq!(cycle(0, 1, 1), 0);
        assert_eq!(cycle(0, 0, -1), 0);
    }

    #[test]
    fn rotation_in_degrees() {
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED};
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_IDENTITY), 0);
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_UNSPECIFIED), 0);
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_ROTATE90), 90);
        assert_eq!(rotation_degrees(DXGI_MODE_ROTATION_ROTATE270), 270);
    }
}
