//! Telling viewers that the host is going away. A host that is killed sends
//! no close, so its viewers only notice at the idle timeout
//! (`net::IDLE_TIMEOUT`). A host that Windows or the user asks to stop closes
//! its sessions with a reason first (`net::CLOSE_SHUTDOWN`, `CLOSE_STOPPED`).
//!
//! Who asks: Ctrl+C and closing the console window (console handler);
//! restart, shutdown and sign-out (WM_ENDSESSION to a hidden window: once a
//! process has loaded user32, Windows no longer sends it the console's
//! CTRL_SHUTDOWN_EVENT); `taskkill` without /f (WM_CLOSE to that window); and
//! for the service's helper, two events the service sets before it stops it
//! (`HELPER_STOP`, `HELPER_SHUTDOWN`).

use std::{
    sync::{Condvar, Mutex, OnceLock},
    time::Duration,
};

use tokio::sync::watch;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM},
        System::{
            Console::{CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler},
            LibraryLoader::GetModuleHandleW,
            Threading::{INFINITE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForMultipleObjects},
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage,
            WM_CLOSE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
    core::{BOOL, HSTRING, w},
};

use crate::{log, net};

/// Set by the service to stop its helper (service stopped, console moved).
pub const HELPER_STOP: &str = r"Global\remote-control-helper-stop";
/// Set by the service when Windows is shutting down.
pub const HELPER_SHUTDOWN: &str = r"Global\remote-control-helper-shutdown";

/// Windows ends the process soon after the handler returns: this long for
/// the sessions' close frames to go out.
const GRACE: Duration = Duration::from_secs(2);

fn code() -> &'static watch::Sender<u32> {
    static CODE: OnceLock<watch::Sender<u32>> = OnceLock::new();
    CODE.get_or_init(|| watch::channel(0).0)
}

static DONE: Mutex<bool> = Mutex::new(false);
static DONE_CV: Condvar = Condvar::new();

/// Asks the host to close its sessions with `close_code` and stop. The first request counts.
pub fn request(close_code: u32) {
    code().send_if_modified(|c| {
        let first = *c == 0;
        if first {
            *c = close_code;
        }
        first
    });
}

pub fn requested() -> bool {
    *code().borrow() != 0
}

/// Resolves with the close code once someone asks the host to go away.
pub async fn wait() -> u32 {
    let mut rx = code().subscribe();
    *rx.wait_for(|c| *c != 0).await.expect("the sender is static")
}

/// The sessions are closed: an asker waiting in `request_and_wait` may return.
pub fn done() {
    *DONE.lock().unwrap() = true;
    DONE_CV.notify_all();
}

fn request_and_wait(close_code: u32) {
    request(close_code);
    let g = DONE.lock().unwrap();
    let _ = DONE_CV.wait_timeout_while(g, GRACE, |d| !*d);
}

/// Installs the console handler and the hidden window; `helper` also
/// listens for the service's events.
pub fn install(helper: bool) {
    unsafe {
        if let Err(e) = SetConsoleCtrlHandler(Some(on_console), true) {
            log!("console handler: {e}");
        }
    }
    let _ = std::thread::Builder::new().name("goaway-window".into()).spawn(|| {
        if let Err(e) = window_thread() {
            log!("end-session window: {e}");
        }
    });
    if helper {
        let _ = std::thread::Builder::new().name("goaway-service".into()).spawn(service_events);
    }
}

unsafe extern "system" fn on_console(ctrl: u32) -> BOOL {
    let c = if ctrl == CTRL_LOGOFF_EVENT || ctrl == CTRL_SHUTDOWN_EVENT { net::CLOSE_SHUTDOWN } else { net::CLOSE_STOPPED };
    log!("console event {ctrl}: closing the sessions");
    request_and_wait(c);
    true.into()
}

fn window_thread() -> windows::core::Result<()> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("remote-control-goaway");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // Top-level and never shown: message-only windows get no WM_ENDSESSION.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            &HSTRING::from("remote-control"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wp.0 != 0 {
                log!("Windows is ending the session: closing the sessions");
                request_and_wait(net::CLOSE_SHUTDOWN);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            log!("asked to close: closing the sessions");
            request(net::CLOSE_STOPPED);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn service_events() {
    let open = |name: &str| unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(name)) };
    let (stop, shutdown) = match (open(HELPER_STOP), open(HELPER_SHUTDOWN)) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            log!("the service's stop events are missing (older service?): {:?} {:?}", a.err(), b.err());
            return;
        }
    };
    let r = unsafe { WaitForMultipleObjects(&[stop, shutdown], false, INFINITE) };
    let c = match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
        0 => net::CLOSE_STOPPED,
        1 => net::CLOSE_SHUTDOWN,
        _ => return log!("waiting for the service's stop events failed: {r:?}"),
    };
    log!("the service asks the helper to {}", if c == net::CLOSE_STOPPED { "stop" } else { "go: Windows is shutting down" });
    request(c);
    unsafe {
        let _ = CloseHandle(stop);
        let _ = CloseHandle(shutdown);
    }
}
