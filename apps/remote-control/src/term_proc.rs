//! The processes of the remote terminal (`term`): the interactive shell in
//! a pseudo console, one-shot commands with their own stdout, stderr and exit
//! code, and file access as the same user.
//!
//! They run with the rights a desktop session has: without the service as
//! the user running this program; in the service's helper (SYSTEM) as the
//! user signed in at the console, elevated when that user is an admin, or as
//! SYSTEM when nobody is signed in. Each process tree is in a job and ends
//! with it.

use std::{
    ffi::c_void,
    fs::File,
    os::windows::io::FromRawHandle,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, anyhow};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0},
        Security::{
            DuplicateTokenEx, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, SecurityImpersonation,
            TOKEN_ALL_ACCESS, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TokenElevationType, TokenElevationTypeLimited,
            TokenLinkedToken, TokenPrimary,
        },
        System::{
            Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole},
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
                TerminateJobObject,
            },
            Pipes::CreatePipe,
            RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
                INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
                WaitForSingleObject,
            },
        },
        UI::Shell::GetUserProfileDirectoryW,
    },
    core::{PCWSTR, PWSTR},
};

/// A handle closed when dropped.
struct Owned(HANDLE);

// Kernel handles may be used from any thread.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// The token of the user signed in at the console, elevated when it is a
/// split admin token; SYSTEM's own when nobody is signed in.
fn console_token() -> Result<Owned> {
    unsafe {
        let mut t = HANDLE::default();
        if WTSQueryUserToken(WTSGetActiveConsoleSessionId(), &mut t).is_ok() {
            let t = Owned(t);
            let mut ty = TOKEN_ELEVATION_TYPE(0);
            let mut len = 0u32;
            let limited = GetTokenInformation(t.0, TokenElevationType, Some(&mut ty as *mut _ as *mut c_void), 4, &mut len).is_ok()
                && ty == TokenElevationTypeLimited;
            if limited {
                let mut linked = TOKEN_LINKED_TOKEN::default();
                let size = size_of::<TOKEN_LINKED_TOKEN>() as u32;
                if GetTokenInformation(t.0, TokenLinkedToken, Some(&mut linked as *mut _ as *mut c_void), size, &mut len).is_ok() {
                    return Ok(Owned(linked.LinkedToken));
                }
            }
            return Ok(t);
        }
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).context("OpenProcessToken")?;
        let own = Owned(own);
        let mut dup = HANDLE::default();
        DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut dup).context("DuplicateTokenEx")?;
        Ok(Owned(dup))
    }
}

/// Who a process runs as: a token (service mode) with its environment.
struct Who {
    token: Option<Owned>,
    env: *mut c_void,
    home: PathBuf,
    path: String,
}

impl Drop for Who {
    fn drop(&mut self) {
        if !self.env.is_null() {
            unsafe {
                let _ = DestroyEnvironmentBlock(self.env);
            }
        }
    }
}

fn who(service: bool) -> Result<Who> {
    if !service {
        return Ok(Who { token: None, env: std::ptr::null_mut(), home: home(false)?, path: std::env::var("PATH").unwrap_or_default() });
    }
    let token = console_token()?;
    let mut env: *mut c_void = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut env, Some(token.0), false) }.context("CreateEnvironmentBlock")?;
    let path = env_var(env, "PATH").unwrap_or_default();
    let home = profile_dir(&token)?;
    Ok(Who { token: Some(token), env, home, path })
}

/// A variable from an environment block (UTF-16 `NAME=value` strings ending with an empty one).
fn env_var(env: *const c_void, name: &str) -> Option<String> {
    let mut p = env as *const u16;
    loop {
        let mut len = 0usize;
        unsafe {
            while *p.add(len) != 0 {
                len += 1;
            }
        }
        if len == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
        if let Some((k, v)) = s.split_once('=')
            && k.eq_ignore_ascii_case(name)
        {
            return Some(v.to_string());
        }
        p = unsafe { p.add(len + 1) };
    }
}

fn profile_dir(token: &Owned) -> Result<PathBuf> {
    let mut buf = vec![0u16; 512];
    let mut len = buf.len() as u32;
    unsafe { GetUserProfileDirectoryW(token.0, Some(PWSTR(buf.as_mut_ptr())), &mut len) }.context("GetUserProfileDirectoryW")?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
}

/// That user's profile folder (relative paths in file copies start there).
pub fn home(service: bool) -> Result<PathBuf> {
    if service {
        return profile_dir(&console_token()?);
    }
    std::env::var_os("USERPROFILE").map(PathBuf::from).ok_or_else(|| anyhow!("no USERPROFILE"))
}

/// Runs `f` on this thread as the user `exec` runs as, for file copies.
pub fn as_user<T>(service: bool, f: impl FnOnce() -> T) -> Result<T> {
    if !service {
        return Ok(f());
    }
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            unsafe {
                let _ = RevertToSelf();
            }
        }
    }
    let token = console_token()?;
    unsafe { ImpersonateLoggedOnUser(token.0) }.context("ImpersonateLoggedOnUser")?;
    let _revert = Revert;
    Ok(f())
}

/// PowerShell 7 when installed, else Windows PowerShell.
fn shell(path: &str) -> PathBuf {
    for dir in path.split(';').filter(|d| !d.trim().is_empty()) {
        let p = Path::new(dir.trim()).join("pwsh.exe");
        if p.is_file() {
            return p;
        }
    }
    let p = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
    if p.is_file() {
        return p;
    }
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A process tree in its own job; killed when dropped.
pub struct Proc {
    process: Owned,
    job: Owned,
}

impl Proc {
    /// Blocks until the process exits; its exit code.
    pub fn wait(&self) -> u32 {
        unsafe {
            if WaitForSingleObject(self.process.0, INFINITE) != WAIT_OBJECT_0 {
                return u32::MAX;
            }
            let mut code = 0u32;
            let _ = GetExitCodeProcess(self.process.0, &mut code);
            code
        }
    }

    /// Ends the whole tree.
    pub fn kill(&self) {
        unsafe {
            let _ = TerminateJobObject(self.job.0, 1);
        }
    }
}

fn new_job() -> Result<Owned> {
    unsafe {
        let job = Owned(CreateJobObjectW(None, PCWSTR::null()).context("CreateJobObjectW")?);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, &info as *const _ as *const c_void, size)
            .context("SetInformationJobObject")?;
        Ok(job)
    }
}

/// What the child's console and standard handles are.
enum Stdio {
    Pty(HPCON),
    /// stdin, stdout, stderr: the child's ends, inheritable.
    Pipes([HANDLE; 3]),
}

/// Starts `cmdline` as `who`, suspended until it is in its job.
fn spawn(who: &Who, cmdline: &str, stdio: &Stdio) -> Result<Proc> {
    unsafe {
        let mut size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        let mut buf = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut c_void);
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut size).context("InitializeProcThreadAttributeList")?;
        struct List(LPPROC_THREAD_ATTRIBUTE_LIST);
        impl Drop for List {
            fn drop(&mut self) {
                unsafe { DeleteProcThreadAttributeList(self.0) }
            }
        }
        let _list = List(list);
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        // Null std handles for a pseudo console's child, or it takes this process's (a log file).
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.lpAttributeList = list;
        let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
        let inherit = match stdio {
            Stdio::Pty(hpc) => {
                UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize, Some(hpc.0 as *const c_void), size_of::<HPCON>(), None, None)
                    .context("UpdateProcThreadAttribute(pseudo console)")?;
                false
            }
            Stdio::Pipes(h) => {
                UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, Some(h.as_ptr() as *const c_void), size_of_val(h), None, None)
                    .context("UpdateProcThreadAttribute(handle list)")?;
                si.StartupInfo.hStdInput = h[0];
                si.StartupInfo.hStdOutput = h[1];
                si.StartupInfo.hStdError = h[2];
                flags |= CREATE_NO_WINDOW;
                true
            }
        };
        let mut desktop = wide("winsta0\\default");
        let env = (!who.env.is_null()).then_some(who.env as *const c_void);
        if env.is_some() {
            flags |= CREATE_UNICODE_ENVIRONMENT;
            si.StartupInfo.lpDesktop = PWSTR(desktop.as_mut_ptr());
        }
        let mut cmd = wide(cmdline);
        let cwd = wide(&who.home.to_string_lossy());
        let mut pi = PROCESS_INFORMATION::default();
        let si_ptr = &si.StartupInfo as *const _;
        let r = match &who.token {
            Some(t) => CreateProcessAsUserW(Some(t.0), PCWSTR::null(), Some(PWSTR(cmd.as_mut_ptr())), None, None, inherit, flags, env, PCWSTR(cwd.as_ptr()), si_ptr, &mut pi),
            None => CreateProcessW(PCWSTR::null(), Some(PWSTR(cmd.as_mut_ptr())), None, None, inherit, flags, env, PCWSTR(cwd.as_ptr()), si_ptr, &mut pi),
        };
        r.context("CreateProcess")?;
        let process = Owned(pi.hProcess);
        let thread = Owned(pi.hThread);
        let job = new_job()?;
        let proc = Proc { process, job };
        if let Err(e) = AssignProcessToJobObject(proc.job.0, proc.process.0) {
            let _ = windows::Win32::System::Threading::TerminateProcess(proc.process.0, 1);
            return Err(anyhow!("AssignProcessToJobObject: {e}"));
        }
        ResumeThread(thread.0);
        Ok(proc)
    }
}


fn pipe() -> Result<(Owned, Owned)> {
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    unsafe { CreatePipe(&mut r, &mut w, None, 0) }.context("CreatePipe")?;
    Ok((Owned(r), Owned(w)))
}

fn file(h: Owned) -> File {
    let f = unsafe { File::from_raw_handle(h.0.0) };
    std::mem::forget(h);
    f
}

/// The interactive shell in a pseudo console.
pub struct Pty {
    proc: Proc,
    hpc: Mutex<Option<isize>>,
}

/// The shell's terminal: keys in, VT output (UTF-8) out.
pub struct PtyIo {
    pub input: File,
    pub output: File,
}

impl Pty {
    pub fn spawn(service: bool, cols: u16, rows: u16) -> Result<(Pty, PtyIo)> {
        Self::spawn_with(service, None, cols, rows)
    }

    /// `cmdline` instead of the shell (for tests).
    pub fn spawn_with(service: bool, cmdline: Option<&str>, cols: u16, rows: u16) -> Result<(Pty, PtyIo)> {
        let who = who(service)?;
        let (in_r, in_w) = pipe()?;
        let (out_r, out_w) = pipe()?;
        let size = COORD { X: cols.max(1) as i16, Y: rows.max(1) as i16 };
        let hpc = unsafe { CreatePseudoConsole(size, in_r.0, out_w.0, 0) }.context("CreatePseudoConsole")?;
        // The console keeps its own copies.
        drop((in_r, out_w));
        let cmd = cmdline.map_or_else(|| format!("\"{}\" -NoLogo", shell(&who.path).display()), str::to_string);
        match spawn(&who, &cmd, &Stdio::Pty(hpc)) {
            Ok(proc) => Ok((Pty { proc, hpc: Mutex::new(Some(hpc.0)) }, PtyIo { input: file(in_w), output: file(out_r) })),
            Err(e) => {
                unsafe { ClosePseudoConsole(hpc) };
                Err(e)
            }
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let Some(h) = *self.hpc.lock().unwrap() else { return Ok(()) };
        let size = COORD { X: cols.max(1) as i16, Y: rows.max(1) as i16 };
        unsafe { ResizePseudoConsole(HPCON(h), size) }.context("ResizePseudoConsole")?;
        Ok(())
    }

    /// Waits for the shell to exit, then closes the pseudo console so the
    /// output reaches its end; the exit code. Someone must keep reading the
    /// output meanwhile: closing waits until it is drained.
    pub fn wait(&self) -> u32 {
        let code = self.proc.wait();
        self.close();
        code
    }

    pub fn kill(&self) {
        self.proc.kill();
    }

    fn close(&self) {
        if let Some(h) = self.hpc.lock().unwrap().take() {
            unsafe { ClosePseudoConsole(HPCON(h)) };
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.proc.kill();
        self.close();
    }
}

/// One command for an agent: its own stdout and stderr, and its exit code.
pub struct Exec {
    pub proc: Proc,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}

/// UTF-8 in and out, also for native programs (they share the hidden console's code page).
const PRELUDE: &str = "[Console]::OutputEncoding=[Console]::InputEncoding=$OutputEncoding=[Text.UTF8Encoding]::new($false)\n\
$ProgressPreference='SilentlyContinue'\nif ($PSStyle) { $PSStyle.OutputRendering='PlainText' }\n$global:LASTEXITCODE=0\n";

/// The exit code: what `exit N` gives; else, when the last command failed,
/// a native program's own code or 1; else 0.
const EPILOGUE: &str = "\n$rc_ok=$?\nif (-not $rc_ok) { if ($LASTEXITCODE) { exit $LASTEXITCODE } else { exit 1 } }\nexit 0\n";

/// Runs `command` in PowerShell (pwsh when installed), in `cwd` or the user's profile folder.
pub fn exec(service: bool, command: &str, cwd: Option<&Path>) -> Result<Exec> {
    let mut who = who(service)?;
    if let Some(c) = cwd {
        who.home = if c.is_absolute() { c.to_path_buf() } else { who.home.join(c) };
    }
    let script = format!("{PRELUDE}{command}{EPILOGUE}");
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let cmd = format!("\"{}\" -NoLogo -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand {}", shell(&who.path).display(), base64(&utf16));
    if cmd.len() > 32_000 {
        return Err(anyhow!("command too long ({} characters; at most about 12000)", command.chars().count()));
    }
    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    let theirs = [in_r.0, out_w.0, err_w.0];
    for h in theirs {
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }.context("SetHandleInformation")?;
    }
    let proc = spawn(&who, &cmd, &Stdio::Pipes(theirs))?;
    drop((in_r, out_w, err_w));
    Ok(Exec { proc, stdin: file(in_w), stdout: file(out_r), stderr: file(err_r) })
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    fn run(cmd: &str) -> (String, String, u32) {
        let e = exec(false, cmd, None).unwrap();
        drop(e.stdin);
        let (mut out, mut err) = (e.stdout, e.stderr);
        let t = std::thread::spawn(move || {
            let mut s = Vec::new();
            err.read_to_end(&mut s).unwrap();
            s
        });
        let mut o = Vec::new();
        out.read_to_end(&mut o).unwrap();
        let code = e.proc.wait();
        (String::from_utf8(o).expect("stdout is UTF-8"), String::from_utf8(t.join().unwrap()).expect("stderr is UTF-8"), code)
    }

    #[test]
    fn exec_keeps_streams_and_exit_code() {
        let (out, err, code) = run("Write-Output '你好'; Write-Error 'oops'; exit 3");
        assert!(out.contains("你好"), "{out:?}");
        assert!(err.contains("oops"), "{err:?}");
        assert_eq!(code, 3);
        let (out, _, code) = run("cmd /c echo 你好");
        assert!(out.contains("你好"), "{out:?}");
        assert_eq!(code, 0);
        assert_eq!(run("cmd /c exit 7").2, 7);
        assert_eq!(run("Get-Item 'C:\\no such thing'").2, 1);
        assert_eq!(run("'a' + 'b'").0.trim(), "ab");
    }

    #[test]
    fn kill_ends_the_tree() {
        let e = exec(false, "cmd /c ping -n 30 127.0.0.1", None).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let t = Instant::now();
        e.proc.kill();
        e.proc.wait();
        let mut rest = Vec::new();
        let mut out = e.stdout;
        // EOF only once ping (the grandchild) is gone too.
        out.read_to_end(&mut rest).unwrap();
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn pty_runs_the_shell() {
        let (pty, io) = Pty::spawn(false, 80, 25).unwrap();
        pty.resize(100, 30).unwrap();
        let mut output = io.output;
        let reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = output.read_to_end(&mut all);
            String::from_utf8_lossy(&all).into_owned()
        });
        let mut input = io.input;
        input.write_all(b"Write-Output ('x'+'y'+'z')\r").unwrap();
        input.write_all(b"exit 5\r").unwrap();
        let t = Instant::now();
        assert_eq!(pty.wait(), 5);
        let text = reader.join().unwrap();
        assert!(text.contains("xyz"), "{text:?}");
        assert!(t.elapsed() < Duration::from_secs(15));
    }

    /// Service mode, run as SYSTEM with someone signed in at the console:
    /// commands, the shell and file access are that user's, elevated.
    #[test]
    #[ignore]
    fn service_mode_as_system() {
        let e = exec(true, "whoami; whoami /groups | Select-String 'S-1-16-12288' | ForEach-Object { 'high' }; Get-Location", None).unwrap();
        drop(e.stdin);
        let (mut out, mut so) = (String::new(), e.stdout);
        so.read_to_string(&mut out).unwrap();
        let code = e.proc.wait();
        println!("exec: code {code}\n{out}");
        assert_eq!(code, 0);
        assert!(!out.contains("system"), "runs as the console user, not SYSTEM");
        assert!(out.contains("high"), "elevated");
        let f = home(true).unwrap().join("rc-as-user-test.txt");
        as_user(true, || std::fs::write(&f, "x")).unwrap().unwrap();
        std::fs::remove_file(&f).unwrap();
        let (pty, io) = Pty::spawn(true, 80, 25).unwrap();
        let mut output = io.output;
        let reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = output.read_to_end(&mut all);
            String::from_utf8_lossy(&all).into_owned()
        });
        let mut input = io.input;
        input.write_all(b"whoami\rexit 6\r").unwrap();
        assert_eq!(pty.wait(), 6);
        let text = reader.join().unwrap();
        println!("pty: {text}");
        assert!(!text.contains("nt authority\\system"));
    }

    #[test]
    fn home_and_as_user() {
        let h = home(false).unwrap();
        assert!(h.is_dir());
        assert_eq!(as_user(false, || 7).unwrap(), 7);
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }
}