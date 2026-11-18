//! Local named-pipe request/response: one request message, one answer
//! message per connection. Used between the helper and the service
//! (Ctrl+Alt+Del) and between the lobby and the node in the helper.

use std::{
    io::{Read, Write},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile},
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientSessionId,
                PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES,
                PIPE_WAIT,
            },
            RemoteDesktop::ProcessIdToSessionId,
            Threading::GetCurrentProcessId,
        },
    },
    core::HSTRING,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// SYSTEM and Administrators.
    Admins,
    /// Also the user signed in on this process's session (the lobby).
    SessionUser,
    /// SYSTEM and the user who runs this process.
    Owner,
}

const BUF: u32 = 256 * 1024;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// The handle is only used by the thread that owns it.
unsafe impl Send for Handle {}

fn read_message(h: HANDLE) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut n = 0u32;
        let r = unsafe { ReadFile(h, Some(&mut buf), Some(&mut n), None) };
        out.extend_from_slice(&buf[..n as usize]);
        match r {
            Ok(()) => return Some(out),
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => continue,
            Err(_) => return None,
        }
    }
}

/// Serves requests forever, each connection on a thread of its own.
pub fn serve(name: &str, access: Access, handler: impl Fn(&str) -> String + Send + Sync + 'static) -> Result<()> {
    let sddl = match access {
        Access::Admins => "D:P(A;;GA;;;SY)(A;;GA;;;BA)",
        // IU: interactive users; the session check below narrows it to this session's.
        Access::SessionUser => "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)",
        Access::Owner => "D:P(A;;GA;;;SY)(A;;GA;;;OW)",
    };
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl), SDDL_REVISION_1, &mut sd, None)
            .context("security descriptor")?;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let mut my_session = 0u32;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut my_session);
    }
    let handler = std::sync::Arc::new(handler);
    let wname = HSTRING::from(name);
    let result = loop {
        let h = unsafe {
            CreateNamedPipeW(
                &wname,
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUF,
                BUF,
                0,
                Some(&sa),
            )
        };
        if h.is_invalid() {
            break Err(windows::core::Error::from_thread()).context("CreateNamedPipeW");
        }
        let h = Handle(h);
        let connected = match unsafe { ConnectNamedPipe(h.0, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if !connected {
            continue;
        }
        if access == Access::SessionUser {
            let mut s = u32::MAX;
            let ok = unsafe { GetNamedPipeClientSessionId(h.0, &mut s) }.is_ok();
            if !ok || (s != my_session && s != 0) {
                continue;
            }
        }
        let handler = handler.clone();
        let _ = std::thread::Builder::new().name("pipe-client".into()).spawn(move || {
            let Some(req) = read_message(h.0) else { return };
            let resp = handler(String::from_utf8_lossy(&req).trim());
            let mut written = 0u32;
            unsafe {
                let _ = WriteFile(h.0, Some(resp.as_bytes()), Some(&mut written), None);
                let _ = FlushFileBuffers(h.0);
            }
            drop(h);
        });
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    result
}

/// Sends one request and returns the reply. Waits up to `wait` for a free
/// pipe instance.
pub fn call(name: &str, msg: &str, wait: Duration) -> Result<String> {
    let until = std::time::Instant::now() + wait;
    let mut f = loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(name) {
            Ok(f) => break f,
            // All instances busy (231) or the server between instances.
            Err(e) if e.raw_os_error() == Some(231) && std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(anyhow!("{name}: {e}")),
        }
    };
    f.write_all(msg.as_bytes())?;
    // The server closes its end after the answer: the end of the answer
    // reads as a broken (109) or disconnected (233) pipe.
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.raw_os_error(), Some(109 | 233)) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
