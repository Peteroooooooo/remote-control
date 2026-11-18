//! With the service, a computer on mains power does not go to sleep (the
//! screen still goes dark), and a viewer that connects lights the screen up
//! (`target-plan` 6.4). No power setting changes: the helper asks Windows
//! while it runs, and the request ends with it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, GetSystemPowerStatus, SYSTEM_POWER_STATUS,
    SetThreadExecutionState,
};

use crate::log;

pub static STAY_AWAKE: AtomicBool = AtomicBool::new(true);

fn on_mains() -> bool {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.ACLineStatus == 1
}

/// Keeps the computer awake while it is plugged in, from a thread of its own
/// (the request belongs to the thread that made it).
pub fn stay_awake_on_mains() {
    let r = std::thread::Builder::new().name("stay-awake".into()).spawn(|| {
        let mut was = None;
        loop {
            let enabled = STAY_AWAKE.load(Ordering::SeqCst);
            let state = if enabled { Some(on_mains()) } else { None };
            if was != state {
                let flags = match state {
                    Some(true) => ES_CONTINUOUS | ES_SYSTEM_REQUIRED,
                    _ => ES_CONTINUOUS,
                };
                unsafe {
                    SetThreadExecutionState(flags);
                }
                match state {
                    Some(true) => log!("on mains power: no sleep while the service runs"),
                    Some(false) => log!("on battery: sleeps as usual"),
                    None => log!("stay awake disabled: sleeps as usual"),
                }
                was = state;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    if let Err(e) = r {
        log!("stay-awake thread: {e}");
    }
}

/// A viewer connected: a dark screen comes on (Windows takes input, even a
/// one-pixel move there and back, as someone at the computer).
pub fn wake_screen() {
    unsafe {
        SetThreadExecutionState(ES_DISPLAY_REQUIRED);
    }
    crate::inject::nudge();
}
