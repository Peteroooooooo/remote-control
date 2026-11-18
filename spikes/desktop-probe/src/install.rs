//! install / uninstall / status. All steps are idempotent.

use std::{ffi::OsString, path::PathBuf, process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use windows_service::{
    service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

use crate::{DATA_DIR, INSTALL_DIR, SERVICE_NAME};

const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";
/// Where install keeps the policy value it replaced.
const BACKUP_KEY: &str = r"SOFTWARE\desktop-probe";

fn require_admin() -> Result<()> {
    if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool() {
        Ok(())
    } else {
        bail!("needs an elevated (administrator) PowerShell")
    }
}

fn installed_exe() -> PathBuf {
    PathBuf::from(INSTALL_DIR).join("desktop-probe.exe")
}

/// Stops the service if it exists. Returns whether it exists.
fn stop_service(mgr: &ServiceManager) -> Result<bool> {
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP;
    let Ok(svc) = mgr.open_service(SERVICE_NAME, access) else { return Ok(false) };
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        for _ in 0..50 {
            if svc.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        println!("  stopped service");
    }
    Ok(true)
}

/// Ends any helper left behind (all desktop-probe.exe except this process).
fn kill_other_probes() {
    let filter = format!("PID ne {}", std::process::id());
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "desktop-probe.exe", "/FI", &filter])
        .output();
}

/// Allows services to call SendSAS; remembers the old value once.
fn set_sas_policy() -> Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (policy, _) = hklm.create_subkey(POLICY_KEY)?;
    let (backup, _) = hklm.create_subkey(BACKUP_KEY)?;
    if backup.get_raw_value("PrevSAS").is_err() && backup.get_raw_value("PrevSASMissing").is_err() {
        match policy.get_value::<u32, _>(POLICY_VALUE) {
            Ok(v) => backup.set_value("PrevSAS", &v)?,
            Err(_) => backup.set_value("PrevSASMissing", &1u32)?,
        }
    }
    policy.set_value(POLICY_VALUE, &1u32)?;
    println!("  set {POLICY_VALUE} = 1 (services may send Ctrl+Alt+Del)");
    Ok(())
}

fn restore_sas_policy() -> Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(backup) = hklm.open_subkey(BACKUP_KEY) else { return Ok(()) };
    let (policy, _) = hklm.create_subkey(POLICY_KEY)?;
    if let Ok(v) = backup.get_value::<u32, _>("PrevSAS") {
        policy.set_value(POLICY_VALUE, &v)?;
        println!("  restored {POLICY_VALUE} = {v}");
    } else if backup.get_raw_value("PrevSASMissing").is_ok() {
        let _ = policy.delete_value(POLICY_VALUE);
        println!("  removed {POLICY_VALUE} (did not exist before)");
    }
    drop(backup);
    hklm.delete_subkey_all(BACKUP_KEY)?;
    Ok(())
}

pub fn install() -> Result<()> {
    require_admin()?;
    let access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let mgr = ServiceManager::local_computer(None::<&str>, access)?;
    let exists = stop_service(&mgr)?;
    kill_other_probes();
    std::fs::create_dir_all(INSTALL_DIR)?;
    std::fs::copy(std::env::current_exe()?, installed_exe()).context("copy exe")?;
    println!("  copied exe to {}", installed_exe().display());
    set_sas_policy()?;
    let svc = if exists {
        mgr.open_service(SERVICE_NAME, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?
    } else {
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("desktop-probe (secure desktop test)"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: installed_exe(),
            launch_arguments: vec![OsString::from("service")],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        let s = mgr.create_service(&info, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
        println!("  created service {SERVICE_NAME} (LocalSystem, auto start)");
        s
    };
    svc.start::<&str>(&[]).context("start service")?;
    std::thread::sleep(Duration::from_secs(2));
    println!("  service state: {:?}", svc.query_status()?.current_state);
    println!("done. screenshots: {}", crate::shots_dir().display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_admin()?;
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if stop_service(&mgr)? {
        mgr.open_service(SERVICE_NAME, ServiceAccess::DELETE)?.delete()?;
        println!("  deleted service {SERVICE_NAME}");
    } else {
        println!("  service not installed");
    }
    kill_other_probes();
    std::thread::sleep(Duration::from_millis(500));
    for dir in [INSTALL_DIR, DATA_DIR] {
        if std::path::Path::new(dir).exists() {
            std::fs::remove_dir_all(dir).with_context(|| format!("remove {dir}"))?;
            println!("  removed {dir}");
        }
    }
    restore_sas_policy()?;
    println!("done.");
    Ok(())
}

pub fn status() -> Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => println!("service: {:?}", s.query_status()?.current_state),
        Err(_) => println!("service: not installed"),
    }
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq desktop-probe.exe", "/FO", "CSV", "/NH"])
        .output()?;
    let n = String::from_utf8_lossy(&out.stdout).matches("desktop-probe.exe").count();
    println!("desktop-probe processes (incl. this one): {n}");
    let latest = std::fs::read_dir(crate::shots_dir())
        .ok()
        .and_then(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name()).max());
    match latest {
        Some(f) => println!("latest screenshot: {}", f.to_string_lossy()),
        None => println!("latest screenshot: none"),
    }
    Ok(())
}
