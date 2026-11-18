//! Per-frame stage timestamps for the same-machine latency test: when
//! `RC_TRACE_DIR` is set, the host and the viewer each append
//! `frame,stage,qpc_us` lines to `<dir>\<role>-trace.csv`. QueryPerformanceCounter
//! is shared by all processes on one machine, so scripts/p1-latency.ps1 can
//! join both files by frame number. Off (one atomic load) otherwise.

use std::{
    io::Write,
    sync::{Mutex, OnceLock},
};

static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// Opens the trace file for `role` ("host" or "view") if tracing is on.
pub fn init(role: &str) {
    FILE.get_or_init(|| {
        let dir = std::env::var_os("RC_TRACE_DIR")?;
        let path = std::path::Path::new(&dir).join(format!("{role}-trace.csv"));
        let f = std::fs::File::create(&path).ok()?;
        crate::log!("tracing frame stages to {}", path.display());
        Some(Mutex::new(f))
    });
}

pub fn qpc_us() -> i64 {
    let (mut v, mut f) = (0i64, 0i64);
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut v);
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut f);
    }
    (v as i128 * 1_000_000 / f.max(1) as i128) as i64
}

pub fn on() -> bool {
    FILE.get().is_some_and(|f| f.is_some())
}

/// `qpc_ticks`: QueryPerformanceCounter ticks, or `None` for now.
pub fn mark(frame: u32, stage: &str, qpc_ticks: Option<i64>) {
    let Some(Some(f)) = FILE.get() else { return };
    let us = match qpc_ticks {
        Some(t) => {
            let mut freq = 0i64;
            unsafe {
                let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq);
            }
            (t as i128 * 1_000_000 / freq.max(1) as i128) as i64
        }
        None => qpc_us(),
    };
    // One write per line, unbuffered: the host is usually ended by being killed.
    let _ = f.lock().unwrap().write_all(format!("{frame},{stage},{us}\n").as_bytes());
}

/// A number that is not a time (frame size), same file format.
pub fn value(frame: u32, name: &str, v: i64) {
    let Some(Some(f)) = FILE.get() else { return };
    let _ = f.lock().unwrap().write_all(format!("{frame},{name},{v}\n").as_bytes());
}

