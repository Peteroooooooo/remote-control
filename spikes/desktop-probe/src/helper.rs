//! The helper: runs as SYSTEM inside the console session, follows the input
//! desktop (Default / Winlogon), saves a screenshot every second and injects input.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::{
    System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
        OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    },
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
            KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
            VIRTUAL_KEY, VK_ESCAPE, VK_RETURN, VK_TAB,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        },
    },
};

use crate::{HELPER_PIPE, log};

/// Attaches the calling thread to the current input desktop; returns its name.
pub fn attach_input_desktop() -> Result<String> {
    unsafe {
        let d = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x1000_0000))
            .context("OpenInputDesktop")?;
        let mut buf = [0u16; 128];
        let mut need = 0u32;
        let _ = GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(d.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut need),
        );
        let name = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
        if SetThreadDesktop(d).is_err() {
            let _ = CloseDesktop(d);
            bail!("SetThreadDesktop({name}) failed");
        }
        // Close the desktop this thread used before, now that it is no longer current.
        CURRENT.with(|c| {
            if let Some(old) = c.replace(Some(d.0 as isize)) {
                let _ = CloseDesktop(windows::Win32::System::StationsAndDesktops::HDESK(old as *mut _));
            }
        });
        Ok(name)
    }
}

thread_local! {
    static CURRENT: std::cell::Cell<Option<isize>> = const { std::cell::Cell::new(None) };
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        bail!("SendInput sent {n}/{} events", inputs.len());
    }
    Ok(())
}

fn mouse(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS, x: i32, y: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: x, dy: y, dwFlags: flags, ..Default::default() } },
    }
}

fn key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, ..Default::default() } },
    }
}

/// Left click at a screen pixel (physical pixels; the primary monitor's top-left is 0,0).
pub fn click(x: i32, y: i32) -> Result<()> {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let norm = |v: i32, origin: i32, size: i32| ((v - origin).clamp(0, size - 1) as i64 * 65535 / (size - 1).max(1) as i64) as i32;
    let (nx, ny) = (norm(x, vx, vw), norm(y, vy, vh));
    let abs = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE | MOUSEEVENTF_VIRTUALDESK;
    send(&[
        mouse(abs, nx, ny),
        mouse(abs | MOUSEEVENTF_LEFTDOWN, nx, ny),
        mouse(abs | MOUSEEVENTF_LEFTUP, nx, ny),
    ])
}

fn press(vk: VIRTUAL_KEY) -> Result<()> {
    send(&[key(vk, 0, KEYBD_EVENT_FLAGS(0)), key(vk, 0, KEYEVENTF_KEYUP)])
}

/// Types text as Unicode characters; {ENTER}, {ESC} and {TAB} press those keys.
pub fn type_text(text: &str) -> Result<()> {
    let mut rest = text;
    while !rest.is_empty() {
        let special = [("{ENTER}", VK_RETURN), ("{ESC}", VK_ESCAPE), ("{TAB}", VK_TAB)]
            .into_iter()
            .find(|(t, _)| rest.starts_with(t));
        if let Some((t, vk)) = special {
            press(vk)?;
            rest = &rest[t.len()..];
            continue;
        }
        let c = rest.chars().next().expect("not empty");
        let mut buf = [0u16; 2];
        for u in c.encode_utf16(&mut buf).iter() {
            send(&[
                key(VIRTUAL_KEY(0), *u, KEYEVENTF_UNICODE),
                key(VIRTUAL_KEY(0), *u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            ])?;
        }
        rest = &rest[c.len_utf8()..];
    }
    Ok(())
}

fn handle(req: &str) -> String {
    if req == "click-uac" {
        let r = crate::uia::click_yes(true);
        log(&format!("click-uac: {r:?}"));
        return match r {
            Ok(s) => format!("ok {s}"),
            Err(e) => format!("error: {e:#}"),
        };
    }
    let desk = match attach_input_desktop() {
        Ok(d) => d,
        Err(e) => return format!("error: {e:#}"),
    };
    let mut it = req.splitn(2, ' ');
    let r = match (it.next(), it.next()) {
        (Some("desk"), None) => return format!("ok {desk}"),
        (Some("click"), Some(xy)) => {
            let v: Vec<i32> = xy.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if v.len() == 2 { click(v[0], v[1]) } else { Err(anyhow::anyhow!("usage: click <x> <y>")) }
        }
        (Some("key"), Some(text)) => type_text(text),
        _ => Err(anyhow::anyhow!("unknown request")),
    };
    log(&format!("input '{req}' on {desk}: {r:?}"));
    match r {
        Ok(()) => format!("ok on desktop {desk}"),
        Err(e) => format!("error on desktop {desk}: {e:#}"),
    }
}

pub fn run() -> Result<()> {
    // Physical pixels everywhere: screenshots, UI Automation rectangles and clicks agree.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    log("helper started");
    std::thread::spawn(|| {
        let r = crate::pipe::serve(HELPER_PIPE, handle);
        log(&format!("helper pipe ended: {r:?}"));
    });
    let mut cap: Option<(String, crate::capture::Capture)> = None;
    let mut next = Instant::now();
    loop {
        let desk = attach_input_desktop().unwrap_or_else(|e| format!("unknown({e})"));
        if cap.as_ref().is_none_or(|(d, _)| *d != desk) {
            log(&format!("input desktop is now {desk}"));
            cap = crate::capture::Capture::new()
                .map_err(|e| log(&format!("capture init on {desk}: {e:#}")))
                .ok()
                .map(|c| (desk.clone(), c));
        }
        if let Some((_, c)) = cap.as_mut() {
            match c.grab(Duration::from_millis(200)) {
                Ok(Some(img)) if Instant::now() >= next => {
                    next = Instant::now() + Duration::from_secs(1);
                    if let Err(e) = crate::capture::save_png(img, &desk) {
                        log(&format!("save png: {e:#}"));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log(&format!("grab on {desk}: {e:#}; recreating"));
                    cap = None;
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
