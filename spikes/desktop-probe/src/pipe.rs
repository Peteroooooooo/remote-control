//! Local named-pipe request/response. Only SYSTEM and Administrators may connect.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HLOCAL, LocalFree},
        Security::{
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile},
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
    },
    core::{HSTRING, w},
};

/// Serves requests forever on the calling thread.
pub fn serve(name: &str, handler: impl Fn(&str) -> String) -> Result<()> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .context("security descriptor")?;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let wname = HSTRING::from(name);
    let result = loop {
        let h = unsafe {
            CreateNamedPipeW(
                &wname,
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                Some(&sa),
            )
        };
        if h.is_invalid() {
            break Err(windows::core::Error::from_thread()).context("CreateNamedPipeW");
        }
        let connected = match unsafe { ConnectNamedPipe(h, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if connected {
            let mut buf = [0u8; 4096];
            let mut n = 0u32;
            if unsafe { ReadFile(h, Some(&mut buf), Some(&mut n), None) }.is_ok() {
                let req = String::from_utf8_lossy(&buf[..n as usize]).to_string();
                let resp = handler(req.trim());
                let mut w = 0u32;
                unsafe {
                    let _ = WriteFile(h, Some(resp.as_bytes()), Some(&mut w), None);
                    let _ = FlushFileBuffers(h);
                }
            }
        }
        unsafe {
            let _ = DisconnectNamedPipe(h);
            let _ = CloseHandle(h);
        }
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    result
}

/// Sends one request and returns the reply.
pub fn call(name: &str, msg: &str) -> Result<String> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name)
        .with_context(|| format!("cannot open {name}: is the service installed and are you admin?"))?;
    f.write_all(msg.as_bytes())?;
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Sends one request and prints the reply.
pub fn request(name: &str, msg: &str) -> Result<()> {
    let reply = call(name, msg)?;
    println!("{reply}");
    if reply.starts_with("error") {
        bail!("request failed");
    }
    Ok(())
}
