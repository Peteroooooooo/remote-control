//! remote-control: one exe that is both the viewer and the host. Without
//! arguments it opens the lobby; the subcommands work without it.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod capture;
mod cc;
mod cli;
mod consent;
mod ctlbar;
mod clipboard;
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
mod pipe;
mod pipeline;
mod power;
mod present;
mod proto;
mod qprate;
mod quality;
mod service;
mod snapshot;
mod stats;
mod store;
mod term;
mod term_proc;
mod theme;
mod toolbar;
mod trace;
mod video_rx;
mod video_tx;
mod view;
mod vp;
mod window;
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
    /// 在一台设备上打开 PowerShell（在当前终端窗口里交互）。
    Shell {
        /// 设备名（大厅里显示的名字，或名字的一部分）或设备码
        device: String,
    },
    /// 在一台设备上运行一条 PowerShell 命令，输出和退出码原样带回（给 AI 和脚本用）。
    #[command(after_help = term::EXEC_HELP)]
    Exec {
        /// 设备名（或名字的一部分）或设备码
        device: String,
        /// 在对方的这个文件夹里运行（默认对方用户的主文件夹）
        #[arg(long)]
        cwd: Option<String>,
        /// 命令，整条写在一个参数里最稳妥
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// 在本机和一台设备之间复制文件或文件夹；远程一边写成 设备:路径。
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
}

fn setup_process() {
    use windows::Win32::{
        Media::timeBeginPeriod,
        UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
    };
    unsafe {
        // Real pixels everywhere: capture sizes, window sizes, pointer positions.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Windows timers tick every 15.6 ms by default, too coarse for pacing.
        timeBeginPeriod(1);
    }
}

fn main() -> Result<()> {
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
