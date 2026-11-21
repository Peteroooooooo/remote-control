//! Replays the viewer's mouse and keyboard on the host with SendInput.
//! Coordinates arrive as 0..=65535 over the streamed display and are mapped to
//! that display's place on the virtual desktop. Keys use scan codes, so the
//! host's keyboard layout decides what a key types, like a real keyboard.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{POINT, RECT},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
            MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
        },
    },
};

pub struct Injector {
    /// The streamed display in desktop pixels.
    display: RECT,
    /// Cached virtual desktop bounds (refreshed every 1 s to avoid 4x
    /// `GetSystemMetrics` syscalls on every mouse move).
    virt: RECT,
    virt_at: Instant,
    keys: HashSet<u16>,
    buttons: [bool; 5],
}

static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// In service mode, input goes to whatever desktop the calling thread is on:
/// attach it to the input desktop first. Cache the desktop check briefly per
/// thread so high-rate mouse moves do not hammer `OpenInputDesktop`, and force
/// a re-check immediately if `SendInput` fails (e.g. UAC / Winlogon switch).
fn follow_desktop(force: bool) {
    static FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let r = if force {
        crate::desktop::follow()
    } else {
        crate::desktop::follow_cached(Duration::from_millis(100))
    };
    if let Err(e) = r {
        let count = FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            crate::log!("input: cannot follow the input desktop ({count} times): {e:#}");
        }
    }
}

/// SendInput reports 0 events when Windows refuses input outright (for example
/// while a secure screen is active). Input aimed at an administrator window
/// from a non-administrator host is dropped without any report (seen on
/// test laptop 2026-11-06), so the host should run as administrator.
fn send(inputs: &[INPUT]) {
    follow_desktop(false);
    let mut n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() && crate::desktop::enabled() {
        // Desktop may have just switched (e.g. UAC prompt or lock screen); re-attach and retry once.
        follow_desktop(true);
        n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    }
    if n as usize != inputs.len() {
        let err = windows::core::Error::from_thread();
        let count = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            crate::log!("input refused by Windows ({count} times): {err}");
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Moves the mouse one pixel and back: input, without a visible change.
pub fn nudge() {
    send(&[mouse(1, 0, 0, MOUSEEVENTF_MOVE), mouse(-1, 0, 0, MOUSEEVENTF_MOVE)]);
}

fn query_virtual_desktop() -> RECT {
    unsafe {
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        RECT {
            left: vx,
            top: vy,
            right: vx + GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            bottom: vy + GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        }
    }
}

impl Injector {
    pub fn new(display: RECT) -> Injector {
        Injector {
            display,
            virt: query_virtual_desktop(),
            virt_at: Instant::now(),
            keys: HashSet::new(),
            buttons: [false; 5],
        }
    }

    pub fn set_display(&mut self, display: RECT) {
        self.display = display;
    }

    /// Stream position (0..=65535) to desktop pixels.
    fn to_desktop(&self, x: u16, y: u16) -> (i32, i32) {
        stream_to_desktop(&self.display, x, y)
    }

    pub fn move_to(&mut self, x: u16, y: u16) {
        let (px, py) = self.to_desktop(x, y);
        if self.virt_at.elapsed() >= Duration::from_secs(1) {
            self.virt = query_virtual_desktop();
            self.virt_at = Instant::now();
        }
        let (ax, ay) = desktop_to_absolute(&self.virt, px, py);
        send(&[mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    pub fn button(&mut self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (4, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
            _ => return,
        };
        self.buttons[button as usize] = down;
        send(&[mouse(0, 0, data, flags)]);
    }

    pub fn wheel(&self, delta: i16, horizontal: bool) {
        let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
        send(&[mouse(0, 0, delta as i32, flags)]);
    }

    pub fn key(&mut self, scan: u16, down: bool) {
        let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
        if scan & 0xFF00 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if down {
            self.keys.insert(scan);
        } else {
            self.keys.remove(&scan);
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: scan & 0xFF,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(&[input]);
    }

    /// Lets go of everything the viewer pressed (focus lost or disconnected).
    pub fn release_all(&mut self) {
        for scan in std::mem::take(&mut self.keys) {
            self.key(scan, false);
            self.keys.remove(&scan);
        }
        for b in 0..5u8 {
            if self.buttons[b as usize] {
                self.button(b, false);
            }
        }
    }

    /// Where the pointer is, as a stream position.
    pub fn cursor(&self) -> (u16, u16) {
        follow_desktop(false);
        let mut p = POINT::default();
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            follow_desktop(true);
            let _ = unsafe { GetCursorPos(&mut p) };
        }
        desktop_to_stream(&self.display, p.x, p.y)
    }
}

/// Stream position (0..=65535 over `display`) to a desktop pixel.
fn stream_to_desktop(d: &RECT, x: u16, y: u16) -> (i32, i32) {
    let w = (d.right - d.left - 1).max(0) as i64;
    let h = (d.bottom - d.top - 1).max(0) as i64;
    (d.left + (x as i64 * w / 65535) as i32, d.top + (y as i64 * h / 65535) as i32)
}

/// Desktop pixel to stream position, clamped to `display`.
fn desktop_to_stream(d: &RECT, px: i32, py: i32) -> (u16, u16) {
    let w = (d.right - d.left - 1).max(1) as i64;
    let h = (d.bottom - d.top - 1).max(1) as i64;
    let x = ((px - d.left).clamp(0, w as i32) as i64 * 65535 / w) as u16;
    let y = ((py - d.top).clamp(0, h as i32) as i64 * 65535 / h) as u16;
    (x, y)
}

/// Desktop pixel to SendInput's absolute 0..=65535 over the virtual desktop
/// `virt` (which starts left of or above 0,0 when a display is there).
fn desktop_to_absolute(virt: &RECT, px: i32, py: i32) -> (i32, i32) {
    let (vw, vh) = ((virt.right - virt.left).max(2) as i64, (virt.bottom - virt.top).max(2) as i64);
    // Windows maps absolute 0..=65535 to pixel = value * size / 65536 (rounded
    // down), so round up here to land on the intended pixel.
    let ax = (((px - virt.left) as i64 * 65536 + vw - 1) / vw).clamp(0, 65535) as i32;
    let ay = (((py - virt.top) as i64 * 65536 + vh - 1) / vh).clamp(0, 65535) as i32;
    (ax, ay)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where Windows puts the pointer for absolute `a` over `virt`.
    fn windows_pixel(virt: &RECT, ax: i32, ay: i32) -> (i32, i32) {
        let (vw, vh) = ((virt.right - virt.left) as i64, (virt.bottom - virt.top) as i64);
        (virt.left + (ax as i64 * vw / 65536) as i32, virt.top + (ay as i64 * vh / 65536) as i32)
    }

    #[test]
    fn maps_onto_a_display_left_of_and_above_the_primary() {
        // Primary 2560x1440 at 0,0; a portrait 1080x1920 display to its left, 300 px higher.
        let left = RECT { left: -1080, top: -300, right: 0, bottom: 1620 };
        let primary = RECT { left: 0, top: 0, right: 2560, bottom: 1440 };
        let virt = RECT { left: -1080, top: -300, right: 2560, bottom: 1620 };
        for (d, corners) in [
            (&left, [(-1080, -300), (-1, 1619)]),
            (&primary, [(0, 0), (2559, 1439)]),
        ] {
            assert_eq!(stream_to_desktop(d, 0, 0), corners[0]);
            assert_eq!(stream_to_desktop(d, 65535, 65535), corners[1]);
            for (x, y) in [(0u16, 0u16), (32768, 100), (65535, 65535), (12345, 54321)] {
                let (px, py) = stream_to_desktop(d, x, y);
                assert!(px >= d.left && px < d.right && py >= d.top && py < d.bottom);
                let (ax, ay) = desktop_to_absolute(&virt, px, py);
                assert_eq!(windows_pixel(&virt, ax, ay), (px, py), "stream ({x}, {y}) on {d:?}");
                let (sx, sy) = desktop_to_stream(d, px, py);
                assert!((sx as i32 - x as i32).abs() <= 65535 / (d.right - d.left - 1) + 1);
                assert!((sy as i32 - y as i32).abs() <= 65535 / (d.bottom - d.top - 1) + 1);
            }
        }
        // The pointer on another display is reported at the streamed display's edge.
        assert_eq!(desktop_to_stream(&primary, -500, 700).0, 0);
        assert_eq!(desktop_to_stream(&left, 100, 2000), (65535, 65535));
    }

    #[test]
    fn every_pixel_of_a_wide_virtual_desktop_is_reachable() {
        // Three 1920-wide displays: absolute units are 2.8 px wide here.
        let virt = RECT { left: -1920, top: 0, right: 3840, bottom: 1080 };
        for px in (-1920..3840).step_by(7).chain([3839]) {
            let (ax, _) = desktop_to_absolute(&virt, px, 0);
            assert_eq!(windows_pixel(&virt, ax, 0).0, px);
        }
    }
}
