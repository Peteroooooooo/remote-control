//! Service mode. `remote-control service` is started by the SCM as SYSTEM in
//! session 0, which has no screen. It keeps one helper (`remote-control
//! helper`, the real host with desktop following on) alive in the active
//! console session, as SYSTEM so the helper may open the Winlogon desktop, and
//! answers Ctrl+Alt+Del requests on a pipe: SendSAS only works when called by
//! a service. The helper runs the node (`node`) with the computer's device
//! key, which install copied here from the user's folder.

use std::{
    ffi::OsString,
    fs::OpenOptions,
    os::windows::io::AsRawHandle,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT},
        Security::{
            DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
            TokenSessionId,
        },
        System::{
            Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle},
            LibraryLoader::{GetProcAddress, LoadLibraryW},
            RemoteDesktop::WTSGetActiveConsoleSessionId,
            Threading::{
                CREATE_NO_WINDOW, CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
                OpenProcessToken, PROCESS_INFORMATION, ResetEvent, STARTUPINFOW, SetEvent, TerminateProcess,
                WaitForSingleObject,
            },
        },
    },
    core::{BOOL, HSTRING, PWSTR, s, w},
};
use windows_service::{
    define_windows_service,
    service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType},
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::{goaway, log, pipe};

pub const SERVICE_NAME: &str = "remote-control";
const SVC_PIPE: &str = r"\\.\pipe\remote-control-svc";

fn known_folder(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(fallback))
}

/// `C:\Program Files\remote-control`: the installed exe.
pub fn install_dir() -> PathBuf {
    known_folder("ProgramFiles", r"C:\Program Files").join("remote-control")
}

/// `C:\ProgramData\remote-control`: device key, allow list and logs of the
/// service. Readable by SYSTEM and Administrators only (set by install).
pub fn data_dir() -> PathBuf {
    known_folder("ProgramData", r"C:\ProgramData").join("remote-control")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join("remote-control.exe")
}

pub fn key_path() -> PathBuf {
    data_dir().join("device.key")
}

/// Sends stdout and stderr (`log!`, tracing) to `<data dir>\<name>.log`;
/// the service and the helper have no console. Keeps one older file.
pub fn redirect_output(name: &str) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.log"));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = std::fs::rename(&path, dir.join(format!("{name}.old.log")));
    }
    redirect_to(&path)?;
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    log!(
        "==== {name} started {:04}-{:02}-{:02} {:02}:{:02}:{:02}, pid {}",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        std::process::id()
    );
    Ok(())
}

/// Sends stdout and stderr to the end of `path`.
pub fn redirect_to(path: &std::path::Path) -> Result<()> {
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    unsafe {
        let h = HANDLE(f.as_raw_handle());
        SetStdHandle(STD_OUTPUT_HANDLE, h)?;
        SetStdHandle(STD_ERROR_HANDLE, h)?;
    }
    // The handle now belongs to the process's standard output.
    std::mem::forget(f);
    Ok(())
}

/// Asks the service to press Ctrl+Alt+Del (from the helper or any admin process).
pub fn request_sas() -> Result<()> {
    let r = pipe::call(SVC_PIPE, "sas", Duration::from_secs(2)).context("Ctrl+Alt+Del needs the service (install-service)")?;
    if r != "ok" {
        bail!("service: {r}");
    }
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

/// The SCM entry point; fails when started from a console.
pub fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("only the service manager can start this")
}

fn service_main(_args: Vec<OsString>) {
    let _ = redirect_output("service");
    if let Err(e) = run_service() {
        log!("service error: {e:#}");
    }
}

/// `supervise` runs until this is not `RUN`.
const RUN: u8 = 0;
const STOP: u8 = 1;
const SHUTDOWN: u8 = 2;

fn run_service() -> Result<()> {
    let stop = Arc::new(AtomicU8::new(RUN));
    let stop2 = stop.clone();
    let handle = service_control_handler::register(SERVICE_NAME, move |ev| match ev {
        ServiceControl::Stop => {
            let _ = stop2.compare_exchange(RUN, STOP, Ordering::SeqCst, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        // Preshutdown comes first, while the network still works.
        ServiceControl::Preshutdown | ServiceControl::Shutdown => {
            stop2.store(SHUTDOWN, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    };
    let accept = ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::PRESHUTDOWN;
    handle.set_service_status(status(ServiceState::Running, accept))?;

    std::thread::spawn(|| {
        let r = pipe::serve(SVC_PIPE, pipe::Access::Admins, |req| {
            let r = match req {
                "sas" => send_sas(),
                _ => return "error: unknown request".into(),
            };
            log!("request '{req}': {r:?}");
            match r {
                Ok(()) => "ok".into(),
                Err(e) => format!("error: {e:#}"),
            }
        });
        log!("pipe ended: {r:?}");
    });

    supervise(&stop);
    log!("service stopping");
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}

/// The events `goaway` in the helper waits on.
struct StopEvents {
    stop: HANDLE,
    shutdown: HANDLE,
}

impl StopEvents {
    fn create() -> Result<StopEvents> {
        let ev = |name: &str| unsafe { CreateEventW(None, true, false, &HSTRING::from(name)) }.with_context(|| format!("create {name}"));
        Ok(StopEvents { stop: ev(goaway::HELPER_STOP)?, shutdown: ev(goaway::HELPER_SHUTDOWN)? })
    }

    fn reset(&self) {
        unsafe {
            let _ = ResetEvent(self.stop);
            let _ = ResetEvent(self.shutdown);
        }
    }
}

/// Keeps a helper running in the active console session until `stop`. A user
/// switch or a remote desktop session takes the console to another session;
/// the helper follows it. A helper that keeps dying is restarted more slowly.
fn supervise(stop: &AtomicU8) {
    let events = match StopEvents::create() {
        Ok(e) => Some(e),
        Err(e) => {
            log!("{e:#}: the helper will be killed without telling its viewers");
            None
        }
    };
    let mut helper: Option<Helper> = None;
    let mut quick_deaths = 0u32;
    let mut next_try = Instant::now();
    let mut last_err = String::new();
    while stop.load(Ordering::SeqCst) == RUN {
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        if let Some(h) = helper.take() {
            if let Some(code) = h.exit_code() {
                let lived = h.started.elapsed();
                log!("helper in session {} exited with code {code} after {:.0} s", h.session, lived.as_secs_f64());
                quick_deaths = if lived < Duration::from_secs(10) { quick_deaths + 1 } else { 0 };
                next_try = Instant::now() + Duration::from_secs(1 << quick_deaths.min(5));
            } else if h.session != console {
                log!("console moved from session {} to {console}: restarting the helper", h.session);
                h.stop(events.as_ref(), false);
            } else {
                helper = Some(h);
            }
        }
        if helper.is_none() && console != u32::MAX && Instant::now() >= next_try {
            if let Some(e) = &events {
                e.reset();
            }
            match Helper::spawn(console) {
                Ok(h) => {
                    log!("helper started in session {console}, pid {}", h.pid);
                    last_err.clear();
                    helper = Some(h);
                }
                Err(e) => {
                    let s = format!("{e:#}");
                    if s != last_err {
                        log!("starting the helper in session {console} failed: {s} (retrying)");
                        last_err = s;
                    }
                    next_try = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some(h) = helper {
        h.stop(events.as_ref(), stop.load(Ordering::SeqCst) == SHUTDOWN);
    }
}

struct Helper {
    session: u32,
    pid: u32,
    process: HANDLE,
    started: Instant,
}

impl Helper {
    /// Starts `remote-control helper` in `session` with a copy of the
    /// service's SYSTEM token, on the interactive window station.
    fn spawn(session: u32) -> Result<Helper> {
        let exe = std::env::current_exe()?;
        let cmd = format!("\"{}\" helper --key \"{}\"", exe.display(), key_path().display());
        let mut cmd: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
        unsafe {
            let mut tok = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut tok).context("OpenProcessToken")?;
            let mut dup = HANDLE::default();
            let r = DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, None, SecurityIdentification, TokenPrimary, &mut dup);
            let _ = CloseHandle(tok);
            r.context("DuplicateTokenEx")?;
            let r = SetTokenInformation(dup, TokenSessionId, &session as *const u32 as *const _, 4)
                .context("SetTokenInformation(TokenSessionId)");
            if let Err(e) = r {
                let _ = CloseHandle(dup);
                return Err(e);
            }
            let si = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();
            let r = CreateProcessAsUserW(
                Some(dup),
                None,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                None,
                &si,
                &mut pi,
            );
            let _ = CloseHandle(dup);
            r.context("CreateProcessAsUserW")?;
            let _ = CloseHandle(pi.hThread);
            Ok(Helper { session, pid: pi.dwProcessId, process: pi.hProcess, started: Instant::now() })
        }
    }

    fn exit_code(&self) -> Option<u32> {
        unsafe {
            if WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT {
                return None;
            }
            let mut code = 0u32;
            let _ = GetExitCodeProcess(self.process, &mut code);
            Some(code)
        }
    }

    fn kill(self) {
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = WaitForSingleObject(self.process, 5000);
        }
    }

    /// Asks the helper to close its sessions with a reason and exit; kills
    /// it if it has not within `STOP_WAIT`.
    fn stop(self, events: Option<&StopEvents>, shutdown: bool) {
        const STOP_WAIT: u32 = 3000;
        if let Some(e) = events {
            let t = Instant::now();
            unsafe {
                let _ = SetEvent(if shutdown { e.shutdown } else { e.stop });
                if WaitForSingleObject(self.process, STOP_WAIT) != WAIT_TIMEOUT {
                    return log!("helper exited {} ms after the stop request", t.elapsed().as_millis());
                }
            }
            log!("helper still running {STOP_WAIT} ms after the stop request: killing it");
        }
        self.kill();
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.process);
        }
    }
}

/// Ctrl+Alt+Del through sas.dll. Windows ignores it unless the
/// SoftwareSASGeneration policy (set by install) allows services to send it.
fn send_sas() -> Result<()> {
    unsafe {
        let lib = LoadLibraryW(w!("sas.dll")).context("load sas.dll")?;
        let f = GetProcAddress(lib, s!("SendSAS")).context("SendSAS not found")?;
        let f: extern "system" fn(BOOL) = std::mem::transmute(f);
        f(false.into());
    }
    Ok(())
}
