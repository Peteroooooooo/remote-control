//! Session plumbing used by the service: start the helper in the console
//! session as SYSTEM, and send Ctrl+Alt+Del.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT},
        Security::{
            DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS,
            TokenPrimary, TokenSessionId,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            LibraryLoader::{GetProcAddress, LoadLibraryW},
            RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
                GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW,
                TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::{BOOL, PWSTR, s, w},
};

pub struct Helper {
    pub session: u32,
    process: HANDLE,
}

// The process handle is only used from the service's supervisor loop.
unsafe impl Send for Helper {}

impl Helper {
    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }
    pub fn kill(self) {
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}

pub fn spawn_helper() -> Result<Helper> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        bail!("no active console session");
    }
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut tok).context("OpenProcessToken")?;
        let mut dup = HANDLE::default();
        let r = DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, None, SecurityIdentification, TokenPrimary, &mut dup);
        let _ = CloseHandle(tok);
        r.context("DuplicateTokenEx")?;
        SetTokenInformation(dup, TokenSessionId, &session as *const u32 as *const _, 4)
            .context("SetTokenInformation(TokenSessionId)")?;
        let exe = std::env::current_exe()?;
        let mut cmd: Vec<u16> = format!("\"{}\" helper", exe.display()).encode_utf16().chain([0]).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
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
        Ok(Helper { session, process: pi.hProcess })
    }
}

/// Starts this exe with `args` as the user logged on at the console, with their
/// normal (not elevated) token. Used by the self test to raise a UAC prompt and to lock.
pub fn run_as_user(args: &str) -> Result<()> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        bail!("no active console session");
    }
    unsafe {
        let mut tok = HANDLE::default();
        WTSQueryUserToken(session, &mut tok).context("WTSQueryUserToken (is someone logged on?)")?;
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, Some(tok), false).is_ok();
        let exe = std::env::current_exe()?;
        let mut cmd: Vec<u16> = format!("\"{}\" {args}", exe.display()).encode_utf16().chain([0]).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
        let si = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessAsUserW(
            Some(tok),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            have_env.then_some(env as *const _),
            None,
            &si,
            &mut pi,
        );
        if have_env {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(tok);
        r.context("CreateProcessAsUserW")?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(())
}

/// Ctrl+Alt+Del via sas.dll. Needs the SoftwareSASGeneration policy set by `install`.
pub fn send_sas() -> Result<()> {
    unsafe {
        let lib = LoadLibraryW(w!("sas.dll")).context("load sas.dll")?;
        let f = GetProcAddress(lib, s!("SendSAS")).context("SendSAS not found")?;
        let f: extern "system" fn(BOOL) = std::mem::transmute(f);
        f(false.into());
    }
    Ok(())
}
