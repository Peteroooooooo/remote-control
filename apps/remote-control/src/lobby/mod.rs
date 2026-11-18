//! The lobby: what a double-click opens. `backend` holds the node and the
//! sessions, `ui` draws them with egui.

mod backend;
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
}

/// Started from Explorer, the console window is this process's alone: hide
/// it and log to `lobby.log` instead. Redirected output stays as it is.
fn leave_own_console(log: &std::path::Path) {
    use windows::Win32::{
        Storage::FileSystem::{FILE_TYPE_CHAR, GetFileType},
        System::Console::{FreeConsole, GetConsoleProcessList, GetStdHandle, STD_OUTPUT_HANDLE},
    };
    let mut pids = [0u32; 4];
    unsafe {
        let console_out = GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|h| GetFileType(h) == FILE_TYPE_CHAR);
        if console_out && GetConsoleProcessList(&mut pids) == 1 {
            let _ = FreeConsole();
            let _ = crate::service::redirect_to(log);
        }
    }
}

/// One lobby per device key: a second one would talk to the same service as
/// another window (each counts as the lobby reopened, see `node`), or go
/// online with the same key. Started again, the lobby brings the open one
/// to the front instead. Returns false then.
fn first_lobby(key: &std::path::Path) -> bool {
    use windows::{
        Win32::{
            Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::CreateMutexW,
            UI::WindowsAndMessaging::{FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow},
        },
        core::{HSTRING, w},
    };
    let name = format!(r"Local\remote-control-lobby-{}", key.to_string_lossy().to_lowercase().replace('\\', "/"));
    unsafe {
        // The handle stays open (never closed) as long as this process runs.
        if CreateMutexW(None, false, &HSTRING::from(name)).is_err() {
            return true;
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        if let Ok(h) = FindWindowW(None, w!("remote control")) {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
        }
    }
    false
}

pub fn run(a: LobbyArgs) -> Result<()> {
    let key = a.key.unwrap_or_else(net::default_key_path);
    if !first_lobby(&key) {
        return Ok(());
    }
    if let Some(dir) = key.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    leave_own_console(&key.with_file_name("lobby.log"));
    crate::goaway::install(false);
    let view_args = a.view_args.map(|s| s.split_whitespace().map(String::from).collect()).unwrap_or_default();
    let b = backend::Backend::start(key, view_args, !a.no_lan)?;
    {
        // Closing the window (or Windows ending the session) ends the
        // sessions this computer hosts, with a reason.
        let b = b.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            let code = rt.block_on(crate::goaway::wait());
            b.goodbye(code);
            crate::goaway::done();
            std::process::exit(0);
        });
    }
    let r = ui::run(b.clone());
    b.goodbye(net::CLOSE_STOPPED);
    r
}
