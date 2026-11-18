//! Following the input desktop (service mode only). The UAC prompt, the lock
//! screen, the Ctrl+Alt+Del screen and the login screen live on the Winlogon
//! desktop; everything else on Default. A thread can only capture and inject
//! on the desktop it is attached to, and only a SYSTEM process may open
//! Winlogon, so the service's helper re-attaches its capture and input threads
//! whenever the input desktop changes. A plain `host` leaves this off.

use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};
use windows::Win32::{
    Foundation::HANDLE,
    System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK,
        OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    },
};

static FOLLOW: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    FOLLOW.store(true, Ordering::SeqCst);
}

pub fn enabled() -> bool {
    FOLLOW.load(Ordering::Relaxed)
}

thread_local! {
    /// The desktop this thread was last attached to by `follow`, and its name.
    static CURRENT: RefCell<Option<(isize, String)>> = const { RefCell::new(None) };
}

/// Opens the input desktop; the caller closes the handle.
fn open_input() -> Result<(HDESK, String)> {
    unsafe {
        // GENERIC_ALL: capture and SendInput both need full access.
        let d = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x1000_0000))
            .context("OpenInputDesktop")?;
        let mut buf = [0u16; 64];
        let mut need = 0u32;
        let _ = GetUserObjectInformationW(
            HANDLE(d.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut need),
        );
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok((d, String::from_utf16_lossy(&buf[..len])))
    }
}

/// Attaches the calling thread to the input desktop if it is not there yet.
/// Returns the input desktop's name, or None when following is off.
/// Fails while the thread owns windows or hooks (Windows refuses the switch).
pub fn follow() -> Result<Option<String>> {
    if !enabled() {
        return Ok(None);
    }
    let (d, name) = open_input()?;
    let same = CURRENT.with(|c| c.borrow().as_ref().is_some_and(|(_, n)| *n == name));
    unsafe {
        if same {
            let _ = CloseDesktop(d);
            return Ok(Some(name));
        }
        if SetThreadDesktop(d).is_err() {
            let e = windows::core::Error::from_thread();
            let _ = CloseDesktop(d);
            bail!("SetThreadDesktop({name}): {e}");
        }
    }
    if let Some((old, _)) = CURRENT.with(|c| c.borrow_mut().replace((d.0 as isize, name.clone()))) {
        unsafe {
            let _ = CloseDesktop(HDESK(old as *mut _));
        }
    }
    let t = std::thread::current();
    crate::log!("thread {} now on desktop {name}", t.name().unwrap_or("?"));
    Ok(Some(name))
}

/// Whether the user's own desktop is the one on screen (not the lock, login,
/// UAC or Ctrl+Alt+Del screen). A process that is not SYSTEM may not open
/// the Winlogon desktop at all, which reads as no.
pub fn user_desktop_shown() -> bool {
    match open_input() {
        Ok((d, name)) => {
            unsafe {
                let _ = CloseDesktop(d);
            }
            name.eq_ignore_ascii_case("Default")
        }
        Err(_) => false,
    }
}

/// Whether the input desktop is no longer the one this thread follows.
pub fn switched() -> bool {
    if !enabled() {
        return false;
    }
    let Ok((d, name)) = open_input() else { return false };
    unsafe {
        let _ = CloseDesktop(d);
    }
    CURRENT.with(|c| c.borrow().as_ref().is_none_or(|(_, n)| *n != name))
}

/// Without the service, why the screen cannot be captured now: a UAC
/// prompt (consent.exe is running) or the lock screen (or another secure
/// screen), both on the Winlogon desktop, which only SYSTEM may capture.
/// `HINT_NONE` when the user's desktop is shown or the service follows it.
pub fn secure_screen_hint() -> u8 {
    use crate::proto::{HINT_LOCKED, HINT_NONE, HINT_UAC};
    if enabled() || user_desktop_shown() {
        return HINT_NONE;
    }
    if process_running("consent.exe") { HINT_UAC } else { HINT_LOCKED }
}

/// Whether a process with this exe name (any case) runs.
fn process_running(exe: &str) -> bool {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS},
    };
    let mut found = false;
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok && !found {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            found = String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(exe);
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_running_processes() {
        let me = std::env::current_exe().unwrap();
        assert!(super::process_running(&me.file_name().unwrap().to_string_lossy().to_uppercase()));
        assert!(!super::process_running("no-such-process-here.exe"));
    }
}
