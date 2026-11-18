//! Where a key event from the viewer's low-level keyboard hook goes: to the
//! host, to the local system, or to one of the session hotkeys.
//! Ctrl+Alt+Shift+Q quits, Ctrl+Alt+Shift+Z gives keyboard and mouse back,
//! Ctrl+Alt+Shift+Left / Right / 1..9 switch the host's display,
//! Ctrl+Alt+End asks for Ctrl+Alt+Del on the host.

use std::collections::HashSet;

const CTRL: [u16; 2] = [0x1D, 0xE01D];
const ALT: [u16; 2] = [0x38, 0xE038];
const SHIFT: [u16; 2] = [0x2A, 0x36];
const VK_Q: u32 = 'Q' as u32;
const VK_Z: u32 = 'Z' as u32;
const VK_END: u32 = 0x23;
const VK_LEFT: u32 = 0x25;
const VK_RIGHT: u32 = 0x27;

/// Ctrl+Alt+Shift+Left / Right / 1..9: which of the host's displays to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorKey {
    Prev,
    Next,
    /// 0-based.
    Pick(u8),
}

/// The display hotkey for virtual key `vk`, when Ctrl, Alt and Shift are held.
/// Only the digit row: with Shift held the number pad sends arrow and paging keys.
pub fn monitor_key(vk: u32) -> Option<MonitorKey> {
    match vk {
        VK_LEFT => Some(MonitorKey::Prev),
        VK_RIGHT => Some(MonitorKey::Next),
        0x31..=0x39 => Some(MonitorKey::Pick((vk - 0x31) as u8)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Let the local system have it.
    Local,
    /// Send to the host and keep it from the local system.
    Remote { scan: u16, down: bool },
    /// Keep it from both.
    Drop,
    Quit,
    Release,
    /// Ctrl+Alt+End: ask the host for Ctrl+Alt+Del.
    Sas,
    Monitor(MonitorKey),
}

/// Remembers which keys went down on the host, so that each key's release
/// goes to the side that saw it pressed: a key held before the hook was
/// installed is released locally, one pressed through the hook on the host.
#[derive(Default)]
pub struct KeyRouter {
    remote: HashSet<u16>,
}

/// Set-1 scan code from the hook's scan code and extended flag; 0 if the
/// event has no usable scan code.
pub fn scan_code(raw: u32, extended: bool) -> u16 {
    match raw {
        // AltGr sends a fake left Ctrl with this code; the host's layout adds its own.
        r if r > 0xFF => 0,
        // The hook marks Right Shift as extended, but it is plain 0x36 in set 1.
        0x2A | 0x36 => raw as u16,
        r if extended => 0xE000 | r as u16,
        r => r as u16,
    }
}

impl KeyRouter {
    /// `local` is what the local system thinks is held (keys it saw before
    /// the hook took over); keys sent to the host never reach it.
    pub fn route(&mut self, vk: u32, scan: u16, down: bool, local: Mods) -> Route {
        if scan == 0 {
            return Route::Drop;
        }
        if !down {
            return if self.remote.remove(&scan) { Route::Remote { scan, down } } else { Route::Local };
        }
        let held = |set: [u16; 2]| set.iter().any(|s| self.remote.contains(s));
        let (ctrl, alt) = (local.ctrl || held(CTRL), local.alt || held(ALT));
        if ctrl && alt && (local.shift || held(SHIFT)) {
            match vk {
                VK_Q => return Route::Quit,
                VK_Z => return Route::Release,
                _ => {}
            }
            if let Some(m) = monitor_key(vk) {
                return Route::Monitor(m);
            }
        }
        if ctrl && alt && vk == VK_END {
            return Route::Sas;
        }
        self.remote.insert(scan);
        Route::Remote { scan, down }
    }

    /// The host was told to let go of everything.
    pub fn clear(&mut self) {
        self.remote.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { ctrl: false, alt: false, shift: false };

    #[test]
    fn keys_pressed_through_the_hook_are_released_on_the_host() {
        let mut r = KeyRouter::default();
        assert_eq!(r.route(0x5B, 0xE05B, true, NONE), Route::Remote { scan: 0xE05B, down: true });
        assert_eq!(r.route(0x5B, 0xE05B, true, NONE), Route::Remote { scan: 0xE05B, down: true });
        assert_eq!(r.route(0x5B, 0xE05B, false, NONE), Route::Remote { scan: 0xE05B, down: false });
        // The local system saw this one go down, so it gets the release.
        assert_eq!(r.route(0x41, 0x1E, false, NONE), Route::Local);
    }

    #[test]
    fn hotkeys_with_modifiers_from_either_side() {
        let mut r = KeyRouter::default();
        for (vk, s) in [(0xA2, 0x1D), (0xA4, 0x38), (0xA0, 0x2A)] {
            assert!(matches!(r.route(vk, s, true, NONE), Route::Remote { .. }));
        }
        assert_eq!(r.route(VK_Z, 0x2C, true, NONE), Route::Release);
        r.clear();
        // Ctrl and Alt were held before the hook; Shift goes through it.
        let local = Mods { ctrl: true, alt: true, shift: false };
        assert!(matches!(r.route(0xA1, 0x36, true, local), Route::Remote { .. }));
        assert_eq!(r.route(VK_Q, 0x10, true, local), Route::Quit);
        // Without all three, Q is just a key.
        let mut r = KeyRouter::default();
        assert_eq!(r.route(VK_Q, 0x10, true, Mods { ctrl: true, alt: true, shift: false }), Route::Remote { scan: 0x10, down: true });
        assert_eq!(r.route(VK_END, 0xE04F, true, Mods { ctrl: true, alt: true, shift: false }), Route::Sas);
        assert_eq!(r.route(VK_END, 0xE04F, true, Mods { ctrl: true, alt: false, shift: false }), Route::Remote { scan: 0xE04F, down: true });
    }

    #[test]
    fn display_hotkeys() {
        let all = Mods { ctrl: true, alt: true, shift: true };
        let mut r = KeyRouter::default();
        assert_eq!(r.route(VK_LEFT, 0xE04B, true, all), Route::Monitor(MonitorKey::Prev));
        assert_eq!(r.route(VK_RIGHT, 0xE04D, true, all), Route::Monitor(MonitorKey::Next));
        assert_eq!(r.route(0x31, 0x02, true, all), Route::Monitor(MonitorKey::Pick(0)));
        assert_eq!(r.route(0x39, 0x0A, true, all), Route::Monitor(MonitorKey::Pick(8)));
        // Released without having gone to the host: the local system gets the release.
        assert_eq!(r.route(VK_RIGHT, 0xE04D, false, all), Route::Local);
        // 0, the number pad and arrows without all three modifiers go to the host.
        assert_eq!(r.route(0x30, 0x0B, true, all), Route::Remote { scan: 0x0B, down: true });
        assert_eq!(r.route(0x61, 0x4F, true, all), Route::Remote { scan: 0x4F, down: true });
        let ctrl_alt = Mods { ctrl: true, alt: true, shift: false };
        assert_eq!(r.route(VK_LEFT, 0xE04B, true, ctrl_alt), Route::Remote { scan: 0xE04B, down: true });
        // Modifiers sent through the hook count too.
        let mut r = KeyRouter::default();
        for (vk, s) in [(0xA2, 0x1D), (0xA4, 0x38), (0xA1, 0x36)] {
            assert!(matches!(r.route(vk, s, true, NONE), Route::Remote { .. }));
        }
        assert_eq!(r.route(0x32, 0x03, true, NONE), Route::Monitor(MonitorKey::Pick(1)));
    }

    #[test]
    fn after_release_nothing_is_left_held_on_the_host() {
        let mut r = KeyRouter::default();
        r.route(0x5B, 0xE05B, true, NONE);
        r.clear();
        assert_eq!(r.route(0x5B, 0xE05B, false, NONE), Route::Local);
    }

    #[test]
    fn scan_codes() {
        assert_eq!(scan_code(0x5B, true), 0xE05B);
        assert_eq!(scan_code(0x1D, false), 0x1D);
        assert_eq!(scan_code(0x36, true), 0x36);
        assert_eq!(scan_code(0x21D, false), 0);
        let mut r = KeyRouter::default();
        assert_eq!(r.route(0xA2, 0, true, NONE), Route::Drop);
    }
}
