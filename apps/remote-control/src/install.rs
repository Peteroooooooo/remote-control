//! `install-service`, `uninstall-service` and `service-status` (admin).
//! Install and uninstall can be run again after a failure; each step skips
//! what is already done.
//!
//! The service runs the exe it was installed from, where it is, with the
//! user's device key, device list and settings where the lobby keeps them:
//! nothing is copied. Older versions copied the exe to Program Files and the
//! data to ProgramData; install and uninstall move the data back and remove
//! those copies.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::PWSTR,
};
use windows_service::{
    service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType},
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
};

use crate::{
    net,
    service::{SERVICE_NAME, old_data_dir, old_install_dir},
    store::{self, Store},
};

const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";
/// Where install keeps the policy value it replaced, for uninstall.
const BACKUP_KEY: &str = r"SOFTWARE\remote-control";
const FIREWALL_RULE: &str = "remote-control-service";
/// Interactive users may start and stop the service (the lobby starts it,
/// its tray menu stops it), only administrators change or remove it.
const SERVICE_SDDL: &str =
    "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";
/// The lobby, hidden in the tray, at logon while the service is installed.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "remote-control";

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
        wait_stopped(&svc)?;
        println!("  stopped the service");
    }
    Ok(true)
}

fn wait_stopped(svc: &windows_service::service::Service) -> Result<()> {
    for _ in 0..75 {
        if svc.query_status()?.current_state == ServiceState::Stopped {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// The exe in a service command line (`"C:\…\remote-control.exe" service --key …`).
fn exe_of(command: &str) -> PathBuf {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return PathBuf::from(rest.split('"').next().unwrap_or(rest));
    }
    let lower = c.to_lowercase();
    match lower.find(".exe") {
        Some(i) => PathBuf::from(&c[..i + 4]),
        None => PathBuf::from(c.split(' ').next().unwrap_or(c)),
    }
}

/// The exe the installed service runs (`None`: no service).
pub fn service_exe() -> Option<PathBuf> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let svc = mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_CONFIG).ok()?;
    Some(exe_of(&svc.query_config().ok()?.executable_path.to_string_lossy()))
}

pub fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The exe the service runs when it is not this one (moved, another build,
/// or a copy an older version made).
pub fn service_elsewhere() -> Option<PathBuf> {
    let theirs = service_exe()?;
    let me = std::env::current_exe().ok()?;
    (!same_file(&me, &theirs)).then_some(theirs)
}

fn is_system(process: HANDLE) -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(process, TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut buf = [0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr().cast()), buf.len() as u32, &mut len).is_ok();
        let _ = CloseHandle(tok);
        if !ok {
            return false;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool()
    }
}

/// Processes running as SYSTEM from one of `exes` (other than this one and
/// `keep`): helpers of a service that did not stop cleanly. The user's own
/// processes from the same exe (the lobby, session windows) stay.
fn leftover_helpers(exes: &[PathBuf], keep: Option<u32>) -> Vec<u32> {
    let exes: Vec<String> = exes.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
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
                    && exes.contains(&String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
                    && is_system(h)
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

pub fn kill_helpers_for(exe: &Path) {
    kill_leftover_helpers(&[exe.to_path_buf()], None);
}

fn kill_leftover_helpers(exes: &[PathBuf], keep: Option<u32>) {
    for pid in leftover_helpers(exes, keep) {
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

fn remove_firewall_rule() {
    let _ = Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")])
        .output();
}

/// Without a rule the firewall drops direct connections to the service: it
/// never shows the "allow access" prompt for a program without a desktop.
fn add_firewall_rule(exe: &Path) -> Result<()> {
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
            &format!("program={}", exe.display()),
            "enable=yes",
            "profile=any",
        ],
    )?;
    println!("  firewall: allowed incoming connections to {}", exe.display());
    Ok(())
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

/// Stops the service until the lobby starts it again (its tray menu's 退出).
/// Works for an interactive non-admin user once SDDL is set.
pub fn stop_service_now() -> Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    if svc.query_status()?.current_state != ServiceState::Stopped {
        svc.stop()?;
        wait_stopped(&svc)?;
    }
    Ok(())
}

/// The lobby starts hidden in the tray at logon (`on`), or not. The lobby
/// sets this itself: the elevated install may run as another user.
pub fn set_autostart(exe: &Path, on: bool) -> Result<()> {
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    let value = format!("\"{}\" lobby --hidden", exe.display());
    if on {
        if run.get_value::<String, _>(RUN_VALUE).ok().as_deref() != Some(value.as_str()) {
            run.set_value(RUN_VALUE, &value)?;
        }
    } else if run.get_raw_value(RUN_VALUE).is_ok() {
        run.delete_value(RUN_VALUE)?;
    }
    Ok(())
}

/// Run elevated by the lobby: the output goes to a file next to the user's
/// key that the lobby reads, since it cannot see this process's console.
pub fn report_to_lobby(user_key: Option<&Path>) -> Result<()> {
    match user_key {
        Some(k) => crate::service::redirect_to(&k.with_file_name("service-setup.log")),
        None => Ok(()),
    }
}

/// An older version's service kept copies of the device list and settings
/// (changed while it ran) in ProgramData: they go back next to the user's
/// key, then its copies of the exe and the data are removed.
fn move_old_copies_back(user_key: &Path) -> Result<()> {
    let old_key = old_data_dir().join("device.key");
    if old_key.exists() {
        if let Some(dir) = user_key.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if !user_key.exists() {
            std::fs::copy(&old_key, user_key).context("copy the device key back")?;
        }
        for (to, from) in [
            (store::devices_path(user_key), store::devices_path(&old_key)),
            (store::settings_path(user_key), store::settings_path(&old_key)),
        ] {
            if from.exists() {
                std::fs::copy(&from, &to).with_context(|| format!("copy {} back", from.display()))?;
            }
        }
        println!("  device list and settings back in {}", user_key.parent().unwrap_or(user_key).display());
    }
    let me = std::env::current_exe()?;
    for dir in [old_install_dir(), old_data_dir()] {
        if !dir.exists() || me.starts_with(&dir) {
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
        println!("  removed {} (an older version's copy)", dir.display());
    }
    Ok(())
}

/// The exes a service's helpers may run from: the service's own and an
/// older version's copy.
fn helper_exes() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = service_exe().into_iter().collect();
    v.push(old_install_dir().join("remote-control.exe"));
    v
}

pub fn install(allow: &[EndpointId], user_key: Option<&Path>, lobby: Option<u32>) -> Result<()> {
    require_admin()?;
    let user_key = user_key.map(Path::to_path_buf).unwrap_or_else(net::default_key_path);
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let exes = helper_exes();
    let exists = stop_service(&mgr)?;
    kill_leftover_helpers(&exes, lobby);
    move_old_copies_back(&user_key)?;
    let id = net::load_key(&user_key)?.public();
    if !allow.is_empty() {
        let mut s = Store::load(&store::devices_path(&user_key))?;
        for a in allow {
            s.add(*a).auto_accept = true;
        }
        s.save()?;
    }
    set_sas_policy()?;
    let exe = std::env::current_exe()?;
    add_firewall_rule(&exe)?;

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "remote-control host".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec!["service".into(), "--key".into(), user_key.clone().into_os_string()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::START | ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG;
    let svc = if exists {
        let s = mgr.open_service(SERVICE_NAME, access)?;
        // The start type stays as the user set it (开机自动在线).
        let start_type = s.query_config()?.start_type;
        s.change_config(&ServiceInfo { start_type, ..info })?;
        println!("  service {SERVICE_NAME} now runs {}", exe.display());
        s
    } else {
        let s = mgr.create_service(&info, access)?;
        println!("  created service {SERVICE_NAME} (LocalSystem, starts with Windows), running {}", exe.display());
        s
    };
    run("sc.exe", &["sdset", SERVICE_NAME, SERVICE_SDDL])?;
    svc.start::<&str>(&[]).context("start the service")?;
    for _ in 0..25 {
        if svc.query_status()?.current_state == ServiceState::Running {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("  service state: {:?}", svc.query_status()?.current_state);
    println!("  the service uses this computer's device key, device list and settings ({})", user_key.display());
    println!("device id {id}");
    println!("logs: {}", user_key.parent().unwrap_or(&user_key).display());
    Ok(())
}

/// The exe, the device list and settings stay where they are.
pub fn uninstall(user_key: Option<&Path>, lobby: Option<u32>) -> Result<()> {
    require_admin()?;
    let user_key = user_key.map(Path::to_path_buf).unwrap_or_else(net::default_key_path);
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let exes = helper_exes();
    if stop_service(&mgr)? {
        mgr.open_service(SERVICE_NAME, ServiceAccess::DELETE)?.delete()?;
        println!("  deleted service {SERVICE_NAME}");
    } else {
        println!("  service not installed");
    }
    kill_leftover_helpers(&exes, lobby);
    move_old_copies_back(&user_key)?;
    restore_sas_policy()?;
    remove_firewall_rule();
    println!("  removed the firewall rule");
    println!("done.");
    Ok(())
}

pub fn status() -> Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG) {
        Ok(s) => {
            println!("service: {:?}", s.query_status()?.current_state);
            println!("command: {}", s.query_config()?.executable_path.display());
        }
        Err(_) => println!("service: not installed"),
    }
    println!("leftover helpers: {:?}", leftover_helpers(&helper_exes(), None));
    let key = net::default_key_path();
    match net::load_key(&key) {
        Ok(k) => println!("device id {}", k.public()),
        Err(e) => println!("device id: cannot read {} ({e:#})", key.display()),
    }
    if let Ok(s) = Store::load(&store::devices_path(&key)) {
        for d in s.listed() {
            println!("  {} {:<16} {}", d.id, d.name, if d.auto_accept { "my computer" } else { "trusted" });
        }
    }
    let policy = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(POLICY_KEY)
        .ok()
        .and_then(|k| k.get_value::<u32, _>(POLICY_VALUE).ok());
    println!("{POLICY_VALUE}: {policy:?}");
    println!("logs: {}", key.parent().unwrap_or(&key).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::exe_of;
    use std::path::PathBuf;

    #[test]
    fn exe_of_a_service_command_line() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            exe_of(r#""D:\a b\remote-control.exe" service --key "C:\Users\x\device.key""#),
            p(r"D:\a b\remote-control.exe")
        );
        assert_eq!(exe_of(r"C:\Program Files\remote-control\remote-control.exe service"), p(r"C:\Program Files\remote-control\remote-control.exe"));
        assert_eq!(exe_of(r"C:\x\rc.EXE"), p(r"C:\x\rc.EXE"));
    }
}
