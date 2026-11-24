//! The lobby: what a double-click opens. `backend` holds the node and the
//! sessions, `ui` draws them with egui, `tray` is its icon in the taskbar's
//! notification area: closing the window hides it, 退出 there quits.

mod backend;
mod host;
mod tray;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::net;

#[derive(Args, Default)]
pub struct LobbyArgs {
    /// Device key file (default: %APPDATA%\remote-control\device.key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Extra arguments for each session's `view` process (for tests).
    #[arg(long, hide = true, allow_hyphen_values = true)]
    view_args: Option<String>,
    /// Neither answer nor search on the LAN (for tests of connection codes).
    #[arg(long, hide = true)]
    no_lan: bool,
    /// Run the node here, never the service's (for tests with `--key`).
    #[arg(long, hide = true)]
    own_node: bool,
    /// Start in the tray, without the window (at logon, with the service).
    #[arg(long, hide = true)]
    hidden: bool,
}

/// Started from Explorer, the console window is this process's alone (or absent
/// with `windows_subsystem = "windows"`): hide it if any and log to `lobby.log`
/// instead. Redirected output stays as it is.
fn leave_own_console(log: &std::path::Path) {
    use windows::Win32::{
        Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_UNKNOWN, GetFileType},
        System::Console::{FreeConsole, GetConsoleProcessList, GetStdHandle, STD_OUTPUT_HANDLE},
    };
    let mut pids = [0u32; 4];
    unsafe {
        let out_type = GetStdHandle(STD_OUTPUT_HANDLE)
            .ok()
            .filter(|h| !h.is_invalid() && !h.0.is_null())
            .map(|h| GetFileType(h))
            .unwrap_or(FILE_TYPE_UNKNOWN);
        if out_type == FILE_TYPE_CHAR && GetConsoleProcessList(&mut pids) == 1 {
            let _ = FreeConsole();
            let _ = crate::service::redirect_to(log);
        } else if out_type == FILE_TYPE_UNKNOWN {
            let _ = crate::service::redirect_to(log);
        }
    }
}

/// One lobby per device key: a second one would talk to the same service as
/// another window (each counts as the lobby reopened, see `node`), or go
/// online with the same key. Started again, the lobby shows the running
/// one's window (also when it is in the tray) instead. Returns false then.
///
/// If a previous lobby is still finishing shutdown (stopping the service after
/// removing its tray icon), we wait for it to release the mutex and then start
/// cleanly on the first click instead of exiting silently.
fn first_lobby(key: &std::path::Path, log: &std::path::Path) -> bool {
    use std::time::{Duration, Instant};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::CreateMutexW,
            UI::WindowsAndMessaging::{FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow},
        },
        core::{HSTRING, w},
    };
    let name = HSTRING::from(format!(
        r"Local\remote-control-lobby-{}",
        key.to_string_lossy().to_lowercase().replace('\\', "/")
    ));
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut left_console = false;
    unsafe {
        loop {
            let Ok(h) = CreateMutexW(None, false, &name) else {
                return true;
            };
            if GetLastError() != ERROR_ALREADY_EXISTS {
                // Keep the handle open for the lifetime of this process.
                return true;
            }
            let _ = CloseHandle(h);
            if tray::open_running() {
                return false;
            }
            if Instant::now() >= deadline {
                if let Ok(win) = FindWindowW(None, w!("Remote Control")) {
                    if IsIconic(win).as_bool() {
                        let _ = ShowWindow(win, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(win);
                }
                return false;
            }
            if !left_console {
                // Hide the console while waiting for a previous instance to finish
                // shutting down (or starting up), so no black window lingers.
                leave_own_console(log);
                left_console = true;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    }
}

pub fn run(a: LobbyArgs) -> Result<()> {
    let key = a.key.unwrap_or_else(net::default_key_path);
    if let Some(dir) = key.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    crate::i18n::init_from_key(&key);
    let log = key.with_file_name("lobby.log");
    if !first_lobby(&key, &log) {
        return Ok(());
    }
    leave_own_console(&log);
    crate::goaway::install(false);
    let view_args = a.view_args.map(|s| s.split_whitespace().map(String::from).collect()).unwrap_or_default();
    let b = backend::Backend::start(key, view_args, !a.no_lan, !a.hidden, a.own_node)?;
    {
        let (shown, quit) = (b.clone(), b.clone());
        tray::spawn(move || shown.set_visible(true), move || quit.quit());
    }
    {
        // Closing the window (or Windows ending the session) ends the
        // sessions this computer hosts, with a reason.
        let b = b.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            let code = rt.block_on(crate::goaway::wait());
            b.flush_undoables();
            b.goodbye(code);
            crate::goaway::done();
            std::process::exit(0);
        });
    }
    let r = ui::run(b.clone(), a.hidden);
    b.flush_undoables();
    b.goodbye(net::CLOSE_STOPPED);
    r
}
