//! `install-service`, `uninstall-service` and `service-status` (admin).
//! Install and uninstall can be run again after a failure; each step skips
//! what is already done.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use windows::{
    Win32::{
        Foundation::CloseHandle,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
                QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::PWSTR,
};
use windows_service::{
    service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType},
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

use crate::{
    net,
    service::{SERVICE_NAME, data_dir, install_dir, installed_exe, key_path},
    store::{self, Store},
};

const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";
/// Where install keeps the policy value it replaced, for uninstall.
const BACKUP_KEY: &str = r"SOFTWARE\remote-control";
const FIREWALL_RULE: &str = "remote-control-service";

fn require_admin() -> Result<()> {
    if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool() {
        Ok(())
    } else {
        bail!("needs an elevated (administrator) shell")
    }
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program).args(args).output().with_context(|| format!("run {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {} {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Stops the service if it exists; returns whether it exists.
fn stop_service(mgr: &ServiceManager) -> Result<bool> {
    let Ok(svc) = mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) else {
        return Ok(false);
    };
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        for _ in 0..75 {
            if svc.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        println!("  stopped the service");
    }
    Ok(true)
}

/// Processes (other than this one and `keep`) whose exe is in the install folder.
fn installed_processes(keep: Option<u32>) -> Vec<u32> {
    let dir = install_dir().to_string_lossy().to_lowercase() + "\\";
    let me = std::process::id();
    let mut found = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return found };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let pid = e.th32ProcessID;
            if pid != me
                && Some(pid) != keep
                && let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok()
                    && String::from_utf16_lossy(&buf[..len as usize]).to_lowercase().starts_with(&dir)
                {
                    found.push(pid);
                }
                let _ = CloseHandle(h);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    found
}

/// Ends helpers left behind by a service that did not stop cleanly.
fn kill_installed_processes(keep: Option<u32>) {
    for pid in installed_processes(keep) {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                if TerminateProcess(h, 1).is_ok() {
                    let _ = WaitForSingleObject(h, 5000);
                    println!("  ended leftover process {pid}");
                }
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Lets services send Ctrl+Alt+Del; remembers the old value once.
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
    // 1 = services may (3 would also allow ease-of-access apps).
    let v = policy.get_value::<u32, _>(POLICY_VALUE).unwrap_or(0);
    if v & 1 == 0 {
        policy.set_value(POLICY_VALUE, &(v | 1))?;
    }
    println!("  {POLICY_VALUE} = {} (services may send Ctrl+Alt+Del)", v | 1);
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
        println!("  removed {POLICY_VALUE} (it did not exist before)");
    }
    drop(backup);
    hklm.delete_subkey_all(BACKUP_KEY)?;
    Ok(())
}

/// The data folder holds the device's secret key: SYSTEM and Administrators only.
fn create_data_dir() -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let d = dir.to_string_lossy();
    run("icacls", &[&d, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/q"])
}

fn remove_firewall_rule() {
    let _ = Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")])
        .output();
}

/// Without a rule the firewall drops direct connections to the service: it
/// never shows the "allow access" prompt for a program without a desktop.
fn add_firewall_rule() -> Result<()> {
    remove_firewall_rule();
    run(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={FIREWALL_RULE}"),
            "dir=in",
            "action=allow",
            &format!("program={}", installed_exe().display()),
            "enable=yes",
            "profile=any",
        ],
    )?;
    println!("  firewall: allowed incoming connections to {}", installed_exe().display());
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether the service is installed (Some) and running. Needs no admin.
pub fn service_running() -> Option<bool> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let svc = mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS).ok()?;
    Some(svc.query_status().ok()?.current_state == ServiceState::Running)
}

/// Changes the service's start type to automatic (AutoStart) or demand-start (Manual).
pub fn set_auto_start(auto: bool) -> Result<()> {
    use windows::Win32::System::Services::{
        ChangeServiceConfigW, ENUM_SERVICE_TYPE, SC_HANDLE, SERVICE_AUTO_START, SERVICE_DEMAND_START,
        SERVICE_ERROR, SERVICE_NO_CHANGE,
    };
    use windows::core::PCWSTR;

    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)?;
    let start_type = if auto { SERVICE_AUTO_START } else { SERVICE_DEMAND_START };
    unsafe {
        ChangeServiceConfigW(
            SC_HANDLE(svc.raw_handle() as *mut _),
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            start_type,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .context("change service start type")?;
    Ok(())
}

/// Starts the service. Works for an interactive non-admin user once SDDL is set.
pub fn start_service() -> Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(SERVICE_NAME, ServiceAccess::START)?;
    match svc.start::<&str>(&[]) {
        Ok(()) => Ok(()),
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1056) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Run elevated by the lobby: the output goes to a file next to the user's
/// key that the lobby reads, since it cannot see this process's console.
pub fn report_to_lobby(user_key: Option<&Path>) -> Result<()> {
    match user_key {
        Some(k) => crate::service::redirect_to(&k.with_file_name("service-setup.log")),
        None => Ok(()),
    }
}

/// The files the service takes over from the user (and gives back).
fn user_files(user_key: &Path) -> [(PathBuf, PathBuf); 2] {
    let svc = key_path();
    [
        (store::devices_path(user_key), store::devices_path(&svc)),
        (store::settings_path(user_key), store::settings_path(&svc)),
    ]
}

/// One computer, one device key: the service goes online with the user's
/// key (made now if there is none), with their device list and settings.
/// Installing over a service already there keeps the service's own list and
/// settings: while it runs, the lobby changes those, not the user's copies.
/// Leftovers of phase 1, when the service had a key of its own, go.
fn take_over(user_key: &Path, allow: &[EndpointId], upgrade: bool) -> Result<EndpointId> {
    let id = net::load_key(user_key)?.public();
    std::fs::copy(user_key, key_path()).context("copy the device key")?;
    for (from, to) in user_files(user_key) {
        if upgrade && to.exists() {
            continue;
        }
        if from.exists() {
            std::fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
        } else {
            let _ = std::fs::remove_file(&to);
        }
    }
    for f in ["allowed.txt", "lobby-id.txt", "service-id.txt", "device.key.online"] {
        let _ = std::fs::remove_file(data_dir().join(f));
    }
    if !allow.is_empty() {
        let mut s = Store::load(&store::devices_path(&key_path()))?;
        for a in allow {
            s.add(*a).auto_accept = true;
        }
        s.save()?;
    }
    println!("  the service uses this computer's device key, device list and settings ({})", user_key.display());
    Ok(id)
}

pub fn install(allow: &[EndpointId], user_key: Option<&Path>, lobby: Option<u32>) -> Result<()> {
    require_admin()?;
    let user_key = user_key.map(Path::to_path_buf).unwrap_or_else(net::default_key_path);
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let exists = stop_service(&mgr)?;
    kill_installed_processes(lobby);

    let me = std::env::current_exe()?;
    if !same_file(&me, &installed_exe()) {
        std::fs::create_dir_all(install_dir())?;
        // A process just ended can hold its exe open for a moment longer.
        let mut tries = 0;
        loop {
            match std::fs::copy(&me, installed_exe()) {
                Ok(_) => break,
                Err(e) if e.raw_os_error() == Some(32) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(e).context("copy the exe"),
            }
        }
        println!("  copied the exe to {}", installed_exe().display());
    }
    create_data_dir()?;
    let id = take_over(&user_key, allow, exists)?;
    set_sas_policy()?;
    add_firewall_rule()?;

    let access = ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let svc = if exists {
        mgr.open_service(SERVICE_NAME, access)?
    } else {
        let info = ServiceInfo {
            name: SERVICE_NAME.into(),
            display_name: "remote-control host".into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: installed_exe(),
            launch_arguments: vec!["service".into()],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        let s = mgr.create_service(&info, access)?;
        println!("  created service {SERVICE_NAME} (LocalSystem, starts with Windows)");
        s
    };
    run(
        "sc.exe",
        &[
            "sdset",
            SERVICE_NAME,
            "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)",
        ],
    )?;
    svc.start::<&str>(&[]).context("start the service")?;
    for _ in 0..25 {
        if svc.query_status()?.current_state == ServiceState::Running {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("  service state: {:?}", svc.query_status()?.current_state);
    println!("device id {id}");
    println!("logs: {}", data_dir().display());
    Ok(())
}

/// Run from the install folder (the lobby was started from there), the
/// program and its firewall rule stay: only the service and its data go.
pub fn uninstall(user_key: Option<&Path>, lobby: Option<u32>) -> Result<()> {
    require_admin()?;
    let user_key = user_key.map(Path::to_path_buf).unwrap_or_else(net::default_key_path);
    let keep_exe = same_file(&std::env::current_exe()?, &installed_exe());
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if stop_service(&mgr)? {
        mgr.open_service(SERVICE_NAME, ServiceAccess::DELETE)?.delete()?;
        println!("  deleted service {SERVICE_NAME}");
    } else {
        println!("  service not installed");
    }
    kill_installed_processes(lobby);
    // The device list and settings changed while the service ran: back to the user.
    if key_path().exists() {
        if let Some(dir) = user_key.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if !user_key.exists() {
            std::fs::copy(key_path(), &user_key).context("copy the device key back")?;
        }
        for (to, from) in user_files(&user_key) {
            if from.exists() {
                std::fs::copy(&from, &to).with_context(|| format!("copy {} back", from.display()))?;
            }
        }
        println!("  device list and settings back in {}", user_key.parent().unwrap_or(&user_key).display());
    }
    for dir in [install_dir(), data_dir()] {
        if keep_exe && dir == install_dir() {
            println!("  kept {} (this program runs from there)", installed_exe().display());
            continue;
        }
        if !dir.exists() {
            continue;
        }
        // A process that just ended may hold its exe or log for a moment.
        let mut r = std::fs::remove_dir_all(&dir);
        for _ in 0..10 {
            if r.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
            r = std::fs::remove_dir_all(&dir);
        }
        r.with_context(|| format!("remove {}", dir.display()))?;
        println!("  removed {}", dir.display());
    }
    restore_sas_policy()?;
    if keep_exe {
        println!("  kept the firewall rule (the program stays)");
    } else {
        remove_firewall_rule();
        println!("  removed the firewall rule");
    }
    println!("done.");
    Ok(())
}

pub fn status() -> Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => println!("service: {:?}", s.query_status()?.current_state),
        Err(_) => println!("service: not installed"),
    }
    println!("installed exe: {}", if installed_exe().exists() { "present" } else { "missing" });
    println!("running from the install folder: {:?}", installed_processes(None));
    match std::fs::read(key_path()) {
        Ok(_) => println!("device id {}", net::load_key(&key_path())?.public()),
        Err(e) => println!("device id: cannot read {} ({e}; needs admin)", key_path().display()),
    }
    if let Ok(s) = Store::load(&store::devices_path(&key_path())) {
        for d in s.listed() {
            println!("  {} {:<16} {}", d.id, d.name, if d.auto_accept { "my computer" } else { "trusted" });
        }
    }
    let policy = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(POLICY_KEY)
        .ok()
        .and_then(|k| k.get_value::<u32, _>(POLICY_VALUE).ok());
    println!("{POLICY_VALUE}: {policy:?}");
    println!("logs: {}", data_dir().display());
    Ok(())
}
