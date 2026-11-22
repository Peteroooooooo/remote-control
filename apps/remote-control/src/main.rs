//! remote-control: one exe that is both the viewer and the host. Without
//! arguments it opens the lobby; the subcommands work without it.

#![cfg_attr(not(test), windows_subsystem = "windows")]

use anyhow::Result;
use clap::{Parser, Subcommand};

mod audio;
mod capture;
mod cc;
mod cli;
mod consent;
mod ctlbar;
mod clipboard;
mod clipfiles;
mod clipsync;
mod cursor;
mod decode;
mod desktop;
mod discovery;
mod encode;
mod gdi;
mod enctest;
mod goaway;
mod gpu;
mod host;
mod i18n;
mod ident;
mod inject;
mod install;
mod keys;
mod lobby;
mod lossim;
mod meet;
mod mf;
mod monitors;
mod net;
mod node;
mod nv12;
mod pipe;
mod pipeline;
mod power;
mod present;
mod proto;
mod qprate;
mod qsv;
mod quality;
mod service;
mod snapshot;
mod stats;
mod store;
mod term;
mod term_proc;
mod theme;
mod toolbar;
mod torgb;
mod trace;
mod video_rx;
mod video_tx;
mod view;
mod vp;
mod wgc;
mod window;
mod wlan;
mod yuv444;

/// `log!(...)`: one line on stdout with the seconds since start.
#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {{
        if !$crate::stats::QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            println!("[{:>8.3}] {}", $crate::stats::now_us() as f64 / 1e6, format!($($t)*));
        }
    }};
}

#[derive(Parser)]
#[command(about = "remote control: remote desktop for Windows")]
struct Cli {
    /// Without a command: the lobby.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the lobby window (what running without a command does).
    Lobby(lobby::LobbyArgs),
    /// Show this device's id.
    Id {
        #[arg(long)]
        key: Option<std::path::PathBuf>,
    },
    /// Be online without the lobby window: devices in the list may connect
    /// (strangers on the LAN and with codes too, after the confirmation popup).
    Host(host::HostArgs),
    /// Connect to a host and show its screen.
    View(view::ViewArgs),
    /// List the device list (and, with --scan, devices on the LAN).
    Devices(cli::DevicesArgs),
    /// Open an interactive PowerShell session on a remote device in this terminal.
    Shell {
        /// Device name (exact or partial match from the lobby) or device id.
        device: String,
    },
    /// Run a single PowerShell command on a remote device and return its output and exit code.
    #[command(after_help = term::EXEC_HELP)]
    Exec {
        /// Device name (exact or partial match) or device id.
        device: String,
        /// Working directory on the remote device (default: remote user's home folder).
        #[arg(long)]
        cwd: Option<String>,
        /// Command to run (passing the full command as one quoted argument is safest).
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Copy files or folders between this computer and a remote device (use `device:path` for the remote side).
    #[command(after_help = term::CP_HELP)]
    Cp {
        from: String,
        to: String,
    },
    /// Send one request to the running lobby, host or service (for tests).
    #[command(hide = true)]
    Ctl(cli::CtlArgs),
    /// Check how the encoder follows bitrate changes (no network; for tests).
    EncTest(enctest::EncTestArgs),
    /// Install the service (admin): starts with Windows, works on the UAC
    /// prompt, lock and login screens, Ctrl+Alt+Del. It runs this exe, where
    /// it is, with this user's device key, device list and settings.
    InstallService {
        /// Device id that may control this computer without asking (repeatable).
        #[arg(long)]
        allow: Vec<iroh::EndpointId>,
        /// The user's key file, next to its device list and settings
        /// (default: this user's); the lobby passes it.
        #[arg(long)]
        from_lobby: Option<std::path::PathBuf>,
        /// The lobby that asked (not ended with the service's leftovers).
        #[arg(long)]
        lobby_pid: Option<u32>,
    },
    /// Remove the service, its policy and firewall rule (admin); the exe,
    /// the device list and settings stay where they are.
    UninstallService {
        #[arg(long)]
        from_lobby: Option<std::path::PathBuf>,
        #[arg(long)]
        lobby_pid: Option<u32>,
    },
    /// Show whether the service is installed and running, and its device id.
    ServiceStatus,
    /// Started by the service manager.
    #[command(hide = true)]
    Service {
        /// The device key (next to the device list and settings) the helper uses.
        #[arg(long)]
        key: Option<std::path::PathBuf>,
    },
    /// Started by the service in the console session: the node, following
    /// the input desktop.
    #[command(hide = true)]
    Helper(host::HostArgs),
    /// Started by the helper as the user: capture this display (`\\.\DISPLAY1`)
    /// with the pointer left out, for the helper.
    #[command(hide = true)]
    CaptureAgent { device: String },
    /// Started by the helper as the user: print the copied files' paths.
    #[command(hide = true)]
    ClipFiles,
    /// Started by the helper as the user: capture system audio for the helper.
    #[command(hide = true)]
    AudioAgent,
}

fn needs_parent_console() -> bool {
    let mut args = std::env::args_os().skip(1);
    let Some(first) = args.next() else {
        return false;
    };
    let is_gui_or_bg = matches!(
        first.to_str(),
        Some("lobby" | "service" | "helper" | "capture-agent" | "clip-files" | "audio-agent")
    );
    if !is_gui_or_bg {
        return true;
    }
    args.any(|a| a == "-h" || a == "--help" || a == "-V" || a == "--version")
}

/// Attaches to the caller's console (if any) for CLI invocations so interactive
/// commands print to PowerShell/CMD while preserving any redirected/piped stdio.
fn attach_parent_console() {
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType},
        System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
            STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        },
    };
    const HANDLES: [STD_HANDLE; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    unsafe {
        let saved: [Option<HANDLE>; 3] = HANDLES.map(|kind| {
            let h = GetStdHandle(kind).ok()?;
            (!h.is_invalid() && !h.0.is_null() && GetFileType(h) != FILE_TYPE_UNKNOWN).then_some(h)
        });
        if saved.iter().all(Option::is_some) {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            for (kind, h) in HANDLES.into_iter().zip(saved) {
                if let Some(h) = h {
                    let _ = SetStdHandle(kind, h);
                }
            }
        }
    }
}

fn setup_process() {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    unsafe {
        // Real pixels everywhere: capture sizes, window sizes, pointer positions.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn main() -> Result<()> {
    if needs_parent_console() {
        attach_parent_console();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    stats::now_us();
    setup_process();
    match Cli::parse().cmd.unwrap_or(Cmd::Lobby(Default::default())) {
        Cmd::Lobby(a) => lobby::run(a),
        Cmd::Id { key } => {
            let k = net::load_key(&key.unwrap_or_else(net::default_key_path))?;
            println!("{}", k.public());
            Ok(())
        }
        Cmd::Host(a) => {
            trace::init("host");
            goaway::install(false);
            let r = runtime()?.block_on(node::run_headless(a.node_options(false)));
            leave_after_goodbye(r)
        }
        Cmd::View(a) => {
            trace::init("view");
            view::run(a)
        }
        Cmd::Devices(a) => cli::devices(a),
        Cmd::Shell { device } => exit_with(term::shell(&device)),
        Cmd::Exec { device, cwd, command } => exit_with(term::exec(&device, &command.join(" "), cwd.as_deref())),
        Cmd::Cp { from, to } => term::cp(&from, &to),
        Cmd::Ctl(a) => cli::ctl(a),
        Cmd::EncTest(a) => enctest::run(a),
        Cmd::InstallService { allow, from_lobby, lobby_pid } => {
            if from_lobby.is_some() {
                install::report_to_lobby(from_lobby.as_deref())?;
            }
            install::install(&allow, from_lobby.as_deref(), lobby_pid)
        }
        Cmd::UninstallService { from_lobby, lobby_pid } => {
            if from_lobby.is_some() {
                install::report_to_lobby(from_lobby.as_deref())?;
            }
            install::uninstall(from_lobby.as_deref(), lobby_pid)
        }
        Cmd::ServiceStatus => install::status(),
        Cmd::Service { key } => service::run(key),
        Cmd::Helper(mut a) => {
            service::set_key(a.key.clone());
            service::redirect_output("helper")?;
            if let Err(e) = service::allow_com_from_user() {
                log!("helper: files copied on the other side cannot be pasted here: {e:#}");
            }
            desktop::enable();
            goaway::install(true);
            power::stay_awake_on_mains();
            a.key = Some(a.key.unwrap_or_else(service::key_path));
            let r = runtime()?.block_on(node::run_headless(a.node_options(true)));
            if let Err(e) = &r {
                log!("helper: {e:#}");
            }
            leave_after_goodbye(r)
        }
        Cmd::CaptureAgent { device } => wgc::serve(&device),
        Cmd::ClipFiles => clipboard::print_copied_files(),
        Cmd::AudioAgent => audio::agent::serve(),
    }
}

/// Exits with the remote command's code; our own failures are 255 with the reason on stderr.
fn exit_with(r: Result<i32>) -> Result<()> {
    match r {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("remote-control: {e:#}");
            std::process::exit(255)
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

/// After the goodbye the session threads may still be capturing or waiting
/// on the clipboard; dropping the runtime would wait for them.
fn leave_after_goodbye(r: Result<()>) -> Result<()> {
    if r.is_ok() && goaway::requested() {
        log!("closed the sessions; exiting");
        std::process::exit(0);
    }
    r
}
