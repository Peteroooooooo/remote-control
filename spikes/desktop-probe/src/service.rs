//! The SYSTEM service: keeps one helper process alive in the active console
//! session and handles Ctrl+Alt+Del requests.

use std::{
    ffi::OsString,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::{SERVICE_NAME, SVC_PIPE, log, session};

define_windows_service!(ffi_service_main, service_main);

pub fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("not started by the SCM")
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        log(&format!("service error: {e:#}"));
    }
}

fn run_service() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let session_changed = Arc::new(AtomicBool::new(true));
    let (s2, c2) = (stop.clone(), session_changed.clone());
    let handle = service_control_handler::register(SERVICE_NAME, move |ev| match ev {
        ServiceControl::Stop => {
            s2.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            c2.store(true, Ordering::SeqCst);
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
    handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SESSION_CHANGE,
    ))?;
    log("service started");

    std::thread::spawn(|| {
        let r = crate::pipe::serve(SVC_PIPE, |req| {
            let r = match req {
                "sas" => session::send_sas().map(|()| "SendSAS called"),
                "uac-test" => session::run_as_user("trigger-uac").map(|()| "UAC prompt requested as the user"),
                "lock" => session::run_as_user("lock").map(|()| "lock requested as the user"),
                _ => return "error: unknown request".into(),
            };
            log(&format!("service request '{req}': {r:?}"));
            match r {
                Ok(s) => format!("ok: {s}"),
                Err(e) => format!("error: {e:#}"),
            }
        });
        log(&format!("service pipe ended: {r:?}"));
    });

    let helper: Arc<Mutex<Option<session::Helper>>> = Arc::new(Mutex::new(None));
    while !stop.load(Ordering::SeqCst) {
        let mut h = helper.lock().unwrap();
        let changed = session_changed.swap(false, Ordering::SeqCst);
        let dead = h.as_ref().is_none_or(|x| !x.alive());
        if changed || dead {
            if let Some(old) = h.take() {
                old.kill();
            }
            match session::spawn_helper() {
                Ok(x) => {
                    log(&format!("helper started in session {}", x.session));
                    *h = Some(x);
                }
                Err(e) => log(&format!("spawn helper failed: {e:#}")),
            }
        }
        drop(h);
        std::thread::sleep(Duration::from_secs(1));
    }
    if let Some(old) = helper.lock().unwrap().take() {
        old.kill();
    }
    log("service stopping");
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}