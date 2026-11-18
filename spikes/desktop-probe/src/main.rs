//! Phase-0 prototype: can a SYSTEM service see and drive the secure desktop
//! (UAC prompt, lock screen, login screen)? See TESTING.md.

mod capture;
mod helper;
mod install;
mod pipe;
mod selftest;
mod service;
mod session;
mod uia;
mod usercmd;

use std::{fs::OpenOptions, io::Write, path::PathBuf};

pub const SERVICE_NAME: &str = "desktop-probe";
pub const HELPER_PIPE: &str = r"\\.\pipe\desktop-probe";
pub const SVC_PIPE: &str = r"\\.\pipe\desktop-probe-svc";
pub const INSTALL_DIR: &str = r"C:\Program Files\desktop-probe";
pub const DATA_DIR: &str = r"C:\ProgramData\desktop-probe";

pub fn shots_dir() -> PathBuf {
    PathBuf::from(DATA_DIR).join("shots")
}

/// Appends one line to probe.log; truncates the file once it passes 1 MB.
pub fn log(msg: &str) {
    let path = PathBuf::from(DATA_DIR).join("probe.log");
    let _ = std::fs::create_dir_all(DATA_DIR);
    let too_big = std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false);
    let mut opts = OpenOptions::new();
    opts.create(true);
    if too_big {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.open(&path) {
        let pid = std::process::id();
        let _ = writeln!(f, "{} [{pid}] {msg}", timestamp());
    }
}

/// Local time as yyyyMMdd-HHmmss.
pub fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

const USAGE: &str = "desktop-probe (phase-0 prototype)
  selftest [--out DIR] run the whole test automatically (asks for admin once)
  install            register and start the SYSTEM service (admin)
  uninstall          stop and remove the service, files and policy (admin)
  status             show service / helper state and the latest screenshot
  click <x> <y>      left click at a screen pixel (admin)
  key <text>         type text; {ENTER} {ESC} {TAB} press keys (admin)
  sas                send Ctrl+Alt+Del (admin)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("selftest") => {
            let out = match args.get(1).map(String::as_str) {
                Some("--out") => args.get(2).map(PathBuf::from),
                _ => None,
            };
            selftest::run(out)
        }
        Some("install") => install::install(),
        Some("uninstall") => install::uninstall(),
        Some("status") => install::status(),
        Some("click") if args.len() == 3 => pipe::request(HELPER_PIPE, &format!("click {} {}", args[1], args[2])),
        Some("key") if args.len() >= 2 => pipe::request(HELPER_PIPE, &format!("key {}", args[1..].join(" "))),
        Some("sas") => pipe::request(SVC_PIPE, "sas"),
        Some("service") => service::run(),
        Some("helper") => helper::run(),
        Some("trigger-uac") => usercmd::trigger_uac(),
        Some("mark") => usercmd::mark(),
        Some("lock") => usercmd::lock(),
        // Debug aid: which button the self test would click on the current desktop.
        Some("find-yes") => uia::click_yes(false).map(|s| println!("{s}")),
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        if args.first().map(String::as_str) == Some("selftest") {
            eprintln!("\n按回车关闭窗口。");
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
        }
        std::process::exit(1);
    }
}
