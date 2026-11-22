//! What the lobby window works with: the node (in this process, or in the
//! service's helper over a pipe), the latest snapshot of it, and the session
//! windows this computer opened. The window only draws `Snap` and calls the
//! methods here.

use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use iroh::{EndpointId, SecretKey};

use crate::{host::HostArgs, install, log, net::{self, PathKind}, node};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snap {
    pub me: String,
    /// The node runs in the service's helper.
    pub service: bool,
    pub name: String,
    /// The name set in the settings (empty: the computer name).
    pub set_name: String,
    pub public: bool,
    pub lan: bool,
    /// The service's options.
    pub boot: bool,
    pub lock: bool,
    pub awake: bool,
    pub language: crate::i18n::LangPref,
    /// Own relays, tried in turn.
    pub relays: Vec<Relay>,
    pub hosting: Option<String>,
    pub asking: Option<String>,
    pub notice: Option<(String, String)>,
    pub suggest: Option<String>,
    pub codes: Vec<Code>,
    pub devs: Vec<Dev>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Relay {
    pub url: String,
    /// "home" (in use), "ok", "fail" or "wait".
    pub state: String,
}

/// How a message to the user feels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Info,
    Warn,
    Err,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub tone: Tone,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    pub token: String,
    pub age_s: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Checking,
    Online { rtt_ms: f64, path: PathKind },
    Offline,
    Unreachable,
    Refused(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dev {
    pub id: EndpointId,
    pub listed: bool,
    pub auto: bool,
    /// Met this run: "lan", "code" or "-".
    pub met: String,
    pub status: Status,
    pub on_lan: bool,
    pub name: String,
    pub ask: Ask,
}

/// A request to remember each other, while it waits for an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    None,
    /// This computer asked.
    Out,
    /// The other side asked.
    In,
}

fn parse_snap(text: &str) -> (Snap, bool) {
    let mut s = Snap::default();
    let mut has_lang = false;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let g = |i: usize| f.get(i).copied().unwrap_or("").to_string();
        match f[0] {
            "me" => {
                s.me = g(1);
                s.service = g(2) == "1";
            }
            "settings" => {
                s.name = g(1);
                s.set_name = g(2);
                let on = |i: usize| g(i) == "1";
                (s.public, s.lan) = (on(3), on(4));
                (s.boot, s.lock, s.awake) = (on(5), on(6), on(7));
                let lang_field = g(8);
                if !lang_field.is_empty() {
                    s.language = crate::i18n::LangPref::parse(&lang_field);
                    has_lang = true;
                }
            }
            "relay" => s.relays.push(Relay { url: g(1), state: g(2) }),
            "hosting" => s.hosting = Some(g(2)),
            "asking" => s.asking = Some(g(1)),
            "notice" => s.notice = Some((g(1), g(2))),
            "suggest" => s.suggest = Some(g(1)),
            "code" => s.codes.push(Code { token: g(1), age_s: g(2).parse().unwrap_or(0) }),
            "dev" => {
                let Ok(id) = g(1).parse() else { continue };
                let st = g(5);
                let status = match st.as_str() {
                    "online" => Status::Online {
                        rtt_ms: g(6).parse().unwrap_or(0.0),
                        path: PathKind::from_code(g(7).parse().unwrap_or(0)),
                    },
                    "offline" => Status::Offline,
                    "unreachable" => Status::Unreachable,
                    s if s.starts_with("refused:") => Status::Refused(s[8..].parse().unwrap_or(1)),
                    _ => Status::Checking,
                };
                s.devs.push(Dev {
                    id,
                    listed: g(2) == "1",
                    auto: g(3) == "1",
                    met: g(4),
                    status,
                    on_lan: g(8) == "1",
                    name: g(9),
                    ask: match g(10).as_str() {
                        "out" => Ask::Out,
                        "in" => Ask::In,
                        _ => Ask::None,
                    },
                });
            }
            _ => {}
        }
    }
    (s, has_lang)
}

#[allow(dead_code)]
fn parse(text: &str) -> Snap {
    parse_snap(text).0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    /// The other side's user is asked.
    Waiting,
    Connected,
    /// Only a public relay left: the picture is paused.
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub state: SessionState,
    pub since: Instant,
}

struct SessionEntry {
    name: String,
    state: SessionState,
    since: Instant,
    child_pid: Option<u32>,
    cancelled: bool,
}

/// A deletion that waits for its toast to go (or for the window/app to close).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Undoable {
    Relay(String),
}

enum Mode {
    Local(node::Node),
    /// The service's helper runs the node.
    Remote,
    /// Neither (between the two, or the local node failed to start).
    Off,
}

struct Sh {
    key: PathBuf,
    lan: bool,
    view_args: Vec<String>,
    rt: tokio::runtime::Runtime,
    mode: Mutex<Mode>,
    snap: Mutex<Snap>,
    /// Bumped before mutations and explicit refreshes so older in-flight polls
    /// never overwrite fresher snapshots.
    snap_gen: AtomicU64,
    /// Why there is no snapshot now.
    trouble: Mutex<Option<String>>,
    repaint: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    sessions: Mutex<BTreeMap<EndpointId, SessionEntry>>,
    /// Installing or removing the service.
    busy: Mutex<Option<String>>,
    /// The lobby's own messages (sessions, install), not yet shown.
    notes: Mutex<Vec<Note>>,
    job: Mutex<Option<Job>>,
    undoables: Mutex<Vec<(u64, Undoable)>>,
    /// Cached SCM status so UI frames never block on SCM RPCs.
    service_installed: AtomicBool,
    service_other: AtomicBool,
    /// Whether the window shows (not only the tray icon).
    visible: AtomicBool,
    /// Counts the window's showings: the node takes a window shown again as
    /// a lobby opened again (`window_id`).
    shown: AtomicU32,
}

/// A request that waits for the network (the own relay), run on a thread.
#[derive(Clone, Debug)]
pub struct Job {
    /// The request's first field.
    pub what: String,
    /// `None` while it runs.
    pub answer: Option<Result<String, String>>,
}

#[derive(Clone)]
pub struct Backend {
    sh: Arc<Sh>,
}

impl Sh {
    fn changed(&self) {
        if let Some(f) = self.repaint.lock().unwrap().as_ref() {
            f();
        }
    }

    fn note(&self, tone: Tone, text: impl Into<String>) {
        let text = text.into();
        log!("lobby: {text}");
        self.notes.lock().unwrap().push(Note { tone, text });
        self.changed();
    }

    /// One request to the node; the answer's text, or the node's error.
    fn request(&self, line: &str) -> Result<String> {
        // The node may wait for the network (the own relay): not under the lock.
        let local = match &*self.mode.lock().unwrap() {
            Mode::Local(n) => Some(n.clone()),
            Mode::Remote => None,
            Mode::Off => return Err(anyhow!("{}", crate::i18n::tr("正在启动", "Starting up"))),
        };
        let answer = match local {
            Some(n) => n.request(line),
            None => crate::pipe::call(node::PIPE, line, Duration::from_secs(1))?,
        };
        match answer.split_once('\t') {
            Some(("ok", rest)) => Ok(rest.to_string()),
            Some(("err", e)) => Err(anyhow!("{e}")),
            _ if answer == "ok" => Ok(String::new()),
            _ => Err(anyhow!("bad answer: {answer}")),
        }
    }

    fn start_local(&self) {
        let host = HostArgs::defaults();
        let mut opts = host.node_options(false);
        opts.key = self.key.clone();
        opts.lan = self.lan;
        match self.rt.block_on(node::Node::start(opts)) {
            Ok(n) => {
                log!("lobby: node running here");
                *self.mode.lock().unwrap() = Mode::Local(n);
                *self.trouble.lock().unwrap() = None;
            }
            Err(e) => {
                log!("lobby: node: {e:#}");
                *self.trouble.lock().unwrap() = Some(format!("{e:#}"));
            }
        }
    }

    fn stop_local(&self) {
        let old = std::mem::replace(&mut *self.mode.lock().unwrap(), Mode::Off);
        if let Mode::Local(n) = old {
            self.rt.block_on(n.stop());
        }
    }

    fn stop_local_handoff(&self) {
        let old = std::mem::replace(&mut *self.mode.lock().unwrap(), Mode::Off);
        if let Mode::Local(n) = old {
            self.rt.block_on(n.stop_for_handoff());
        }
    }

    /// What the node knows this window by (`snapshot`, `lobby-closed`): the
    /// process and the showing.
    fn window_id(&self) -> String {
        format!("{}-{}", std::process::id(), self.shown.load(Ordering::SeqCst))
    }

    fn snapshot_query(&self) -> String {
        if self.visible.load(Ordering::SeqCst) {
            format!("snapshot\t{}", self.window_id())
        } else {
            "peek".to_string()
        }
    }

    fn refresh_service_cache(&self) -> bool {
        let state = install::service_running();
        let installed = state.is_some();
        let running = state == Some(true);
        let other = installed && install::service_elsewhere().is_some();
        let prev_installed = self.service_installed.swap(installed, Ordering::SeqCst);
        let prev_other = self.service_other.swap(other, Ordering::SeqCst);
        if prev_installed != installed || prev_other != other {
            self.changed();
        }
        running
    }

    /// While the service runs this exe, the lobby starts hidden at logon if
    /// the service starts with Windows (开机自动在线); otherwise not.
    fn sync_autostart(&self, last: &mut Option<bool>) {
        let Ok(me) = std::env::current_exe() else { return };
        let want = match install::service_exe() {
            Some(exe) if install::same_file(&me, &exe) => self.snap.lock().unwrap().boot,
            Some(_) => return,
            None => false,
        };
        if *last != Some(want) {
            match install::set_autostart(&me, want) {
                Ok(()) => *last = Some(want),
                Err(e) => log!("lobby: start at logon: {e:#}"),
            }
        }
    }

    /// Keeps the mode in line with the service (also when it is installed or
    /// removed from the command line) and the snapshot fresh.
    fn poll(self: &Arc<Self>) {
        let mut last_mode_check = Instant::now() - Duration::from_secs(10);
        let mut remote_failing: Option<Instant> = None;
        let (mut last_autostart_check, mut autostart) = (Instant::now() - Duration::from_secs(60), None);
        loop {
            if last_autostart_check.elapsed() >= Duration::from_secs(5) && self.busy.lock().unwrap().is_none() {
                last_autostart_check = Instant::now();
                self.sync_autostart(&mut autostart);
            }
            if last_mode_check.elapsed() >= Duration::from_secs(1) {
                last_mode_check = Instant::now();
                let running = self.refresh_service_cache();
                let busy = self.busy.lock().unwrap().is_some();
                let mode = match &*self.mode.lock().unwrap() {
                    Mode::Local(_) => 0,
                    Mode::Remote => 1,
                    Mode::Off => 2,
                };
                match (running, mode) {
                    (true, 0) => {
                        log!("lobby: the service runs now: it takes over");
                        self.stop_local_handoff();
                        *self.mode.lock().unwrap() = Mode::Remote;
                    }
                    (true, 2) => *self.mode.lock().unwrap() = Mode::Remote,
                    (false, 1) if !busy => {
                        // Give a restarting service a moment.
                        if remote_failing.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
                            log!("lobby: the service is gone: going online here");
                            *self.mode.lock().unwrap() = Mode::Off;
                            self.start_local();
                        }
                    }
                    (false, 2) if !busy => self.start_local(),
                    _ => {}
                }
            }
            // Hidden in the tray, the window does not count as open (`node::lobby_open`).
            let ask = self.snapshot_query();
            let seq = self.snap_gen.load(Ordering::SeqCst);
            match self.request(&ask) {
                Ok(text) => {
                    remote_failing = None;
                    *self.trouble.lock().unwrap() = None;
                    let (s, has_lang) = parse_snap(&text);
                    let mut cur = self.snap.lock().unwrap();
                    if self.snap_gen.load(Ordering::SeqCst) == seq {
                        if has_lang {
                            crate::i18n::set_pref(s.language);
                        }
                        if *cur != s {
                            *cur = s;
                            drop(cur);
                            self.changed();
                        }
                    }
                }
                Err(e) => {
                    let first = remote_failing.is_none();
                    remote_failing.get_or_insert_with(Instant::now);
                    if first {
                        log!("lobby: snapshot: {e:#}");
                    }
                    let is_remote = matches!(*self.mode.lock().unwrap(), Mode::Remote);
                    if is_remote {
                        *self.trouble.lock().unwrap() = Some(
                            crate::i18n::tr("正在连接后台服务…", "Connecting to background service…").into(),
                        );
                        self.changed();
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Backend {
    pub fn start(key: PathBuf, view_args: Vec<String>, lan: bool, visible: bool) -> Result<Backend> {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let sh = Arc::new(Sh {
            key,
            lan,
            view_args,
            rt,
            mode: Mutex::new(Mode::Off),
            snap: Mutex::default(),
            snap_gen: AtomicU64::new(0),
            trouble: Mutex::new(Some(crate::i18n::tr("正在启动…", "Starting up…").into())),
            repaint: Mutex::default(),
            sessions: Mutex::default(),
            busy: Mutex::default(),
            notes: Mutex::default(),
            job: Mutex::default(),
            undoables: Mutex::default(),
            service_installed: AtomicBool::new(false),
            service_other: AtomicBool::new(false),
            visible: visible.into(),
            shown: Default::default(),
        });
        if service_up() {
            log!("lobby: the service runs: talking to it");
            *sh.mode.lock().unwrap() = Mode::Remote;
        } else {
            sh.start_local();
        }
        sh.refresh_service_cache();
        let s2 = sh.clone();
        std::thread::Builder::new().name("lobby-poll".into()).spawn(move || s2.poll())?;
        // `remote-control ctl` reaches the node in this process here.
        let s3 = sh.clone();
        let pipe = format!(r"\\.\pipe\remote-control-node-s{}", my_session());
        std::thread::Builder::new().name("lobby-pipe".into()).spawn(move || {
            let b = Backend { sh: s3 };
            let r = crate::pipe::serve(&pipe, crate::pipe::Access::Owner, move |req| {
                // `control <id>`: what the row's button does (for tests).
                if let Some(id) = req.strip_prefix("control\t").and_then(|i| i.trim().parse::<EndpointId>().ok()) {
                    let name = b.snap().devs.iter().find(|d| d.id == id).map_or(String::new(), |d| d.name.clone());
                    b.control(id, &name);
                    return "ok\t".into();
                }
                match b.sh.request(req) {
                    Ok(t) => format!("ok\t{t}"),
                    Err(e) => format!("err\t{e:#}"),
                }
            });
            log!("lobby pipe: {r:?}");
        })?;
        Ok(Backend { sh })
    }

    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.sh.repaint.lock().unwrap() = Some(Box::new(f));
    }

    pub fn snap(&self) -> Snap {
        self.sh.snap.lock().unwrap().clone()
    }

    pub fn trouble(&self) -> Option<String> {
        self.sh.trouble.lock().unwrap().clone()
    }

    pub fn busy(&self) -> Option<String> {
        self.sh.busy.lock().unwrap().clone()
    }

    /// Messages for the user since the last call.
    pub fn take_notices(&self) -> Vec<Note> {
        std::mem::take(&mut *self.sh.notes.lock().unwrap())
    }

    pub fn sessions(&self) -> BTreeMap<EndpointId, (String, SessionInfo)> {
        self.sh
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| (*id, (e.name.clone(), SessionInfo { state: e.state, since: e.since })))
            .collect()
    }

    /// A request whose answer only matters if it failed (shown as a notice).
    pub fn act(&self, line: &str) {
        self.sh.snap_gen.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = self.sh.request(line) {
            self.sh.note(Tone::Err, format!("{e:#}"));
        }
        self.refresh();
    }

    pub fn request(&self, line: &str) -> Result<String> {
        self.sh.snap_gen.fetch_add(1, Ordering::SeqCst);
        let r = self.sh.request(line);
        self.refresh();
        r
    }

    /// Runs `line` on a thread; `job` tells how it went.
    pub fn start_job(&self, line: String) {
        let what = line.split('\t').next().unwrap_or("").to_string();
        *self.sh.job.lock().unwrap() = Some(Job { what, answer: None });
        self.sh.changed();
        let b = self.clone();
        let r = std::thread::Builder::new().name("job".into()).spawn(move || {
            b.sh.snap_gen.fetch_add(1, Ordering::SeqCst);
            let r = b.sh.request(&line).map_err(|e| format!("{e:#}"));
            if let Some(j) = b.sh.job.lock().unwrap().as_mut() {
                j.answer = Some(r);
            }
            b.refresh();
        });
        if let Err(e) = r {
            *self.sh.job.lock().unwrap() = None;
            self.sh.note(Tone::Err, format!("{e}"));
        }
    }

    pub fn job(&self) -> Option<Job> {
        self.sh.job.lock().unwrap().clone()
    }

    pub fn clear_job(&self) {
        let mut j = self.sh.job.lock().unwrap();
        if j.as_ref().is_some_and(|j| j.answer.is_some()) {
            *j = None;
        }
    }

    pub fn defer_undoable(&self, id: u64, item: Undoable) {
        self.sh.undoables.lock().unwrap().push((id, item));
    }

    pub fn cancel_undoable(&self, id: u64) {
        self.sh.undoables.lock().unwrap().retain(|(p, _)| *p != id);
    }

    pub fn commit_undoable(&self, id: u64) {
        let item = {
            let mut u = self.sh.undoables.lock().unwrap();
            u.iter().position(|(p, _)| *p == id).map(|i| u.remove(i).1)
        };
        if let Some(what) = item {
            self.carry_out_undoable(what);
        }
    }

    pub fn flush_undoables(&self) {
        let items = std::mem::take(&mut *self.sh.undoables.lock().unwrap());
        for (_, what) in items {
            self.carry_out_undoable(what);
        }
    }

    pub fn has_pending_relay(&self, url: &str) -> bool {
        self.sh.undoables.lock().unwrap().iter().any(|(_, w)| matches!(w, Undoable::Relay(u) if u == url))
    }

    fn carry_out_undoable(&self, what: Undoable) {
        match what {
            Undoable::Relay(url) => self.act(&format!("relay-remove\t{url}")),
        }
    }

    pub fn set_language(&self, lang: crate::i18n::LangPref) {
        crate::i18n::set_pref(lang);
        self.act(&format!("set-language\t{}", lang.as_str()));
    }

    /// Takes a new snapshot now rather than at the next poll.
    fn refresh(&self) {
        let seq = self.sh.snap_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let ask = self.sh.snapshot_query();
        if let Ok(t) = self.sh.request(&ask) {
            let (s, has_lang) = parse_snap(&t);
            let mut cur = self.sh.snap.lock().unwrap();
            if self.sh.snap_gen.load(Ordering::SeqCst) == seq {
                if has_lang {
                    crate::i18n::set_pref(s.language);
                }
                *cur = s;
            }
        }
        self.sh.changed();
    }

    /// Opens a session window to `id` (one per device).
    pub fn control(&self, id: EndpointId, name: &str) {
        {
            let mut s = self.sh.sessions.lock().unwrap();
            if s.contains_key(&id) {
                return;
            }
            s.insert(
                id,
                SessionEntry {
                    name: name.to_string(),
                    state: SessionState::Connecting,
                    since: Instant::now(),
                    child_pid: None,
                    cancelled: false,
                },
            );
        }
        self.sh.changed();
        let sh = self.sh.clone();
        let name = name.to_string();
        let r = std::thread::Builder::new().name("session".into()).spawn(move || {
            if let Err(e) = session(&sh, id, &name) {
                let msg = if crate::i18n::is_en() {
                    format!("Could not control \"{name}\": {e:#}")
                } else {
                    format!("没能控制「{name}」：{e:#}")
                };
                sh.note(Tone::Err, msg);
            }
            sh.sessions.lock().unwrap().remove(&id);
            sh.changed();
        });
        if let Err(e) = r {
            self.sh.sessions.lock().unwrap().remove(&id);
            self.sh.changed();
            self.sh.note(Tone::Err, format!("{e}"));
        }
    }

    pub fn cancel_control(&self, id: EndpointId) {
        let pid = {
            let mut s = self.sh.sessions.lock().unwrap();
            let Some(e) = s.get_mut(&id) else { return };
            e.cancelled = true;
            e.child_pid.take()
        };
        if let Some(pid) = pid {
            use windows::Win32::{
                Foundation::CloseHandle,
                System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
            };
            unsafe {
                if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                    let _ = TerminateProcess(h, 0);
                    let _ = CloseHandle(h);
                }
            }
        }
        self.sh.changed();
    }

    /// Installs (`true`) or removes the service: one UAC prompt.
    pub fn service(&self, install: bool) {
        *self.sh.busy.lock().unwrap() = Some(
            if install {
                crate::i18n::tr(
                    "正在安装后台服务，请在弹窗里点「是」",
                    "Installing background service — click \"Yes\" in the UAC prompt",
                )
            } else {
                crate::i18n::tr(
                    "正在卸载后台服务，请在弹窗里点「是」",
                    "Uninstalling background service — click \"Yes\" in the UAC prompt",
                )
            }
            .into(),
        );
        self.sh.changed();
        let sh = self.sh.clone();
        let _ = std::thread::Builder::new().name("service-setup".into()).spawn(move || {
            if install {
                // The service goes online with this computer's device key: this
                // process must be offline first.
                sh.stop_local_handoff();
            }
            let update = install && install::service_running().is_some();
            let r = run_elevated(install, &sh.key);
            match r {
                Ok(_) if update => sh.note(
                    Tone::Ok,
                    crate::i18n::tr("后台服务已改用这个程序", "Background service updated to use this executable"),
                ),
                Ok(t) => sh.note(Tone::Ok, t),
                Err(e) => {
                    let msg = if crate::i18n::is_en() {
                        format!("{} failed: {e:#}", if install { "Install" } else { "Uninstall" })
                    } else {
                        format!("{}失败：{e:#}", if install { "安装" } else { "卸载" })
                    };
                    sh.note(Tone::Err, msg);
                }
            }
            if !install {
                sh.stop_local();
                *sh.mode.lock().unwrap() = Mode::Off;
            }
            sh.refresh_service_cache();
            *sh.busy.lock().unwrap() = None;
            sh.changed();
        });
    }

    pub fn service_installed(&self) -> bool {
        self.sh.service_installed.load(Ordering::SeqCst)
    }

    pub fn service_other(&self) -> bool {
        self.sh.service_other.load(Ordering::SeqCst)
    }

    /// The window shows (`true`) or went to the tray. Hidden, it does not
    /// count as open: connection codes are void and the temporary devices
    /// forgotten, as when it closed before there was a tray.
    pub fn set_visible(&self, on: bool) {
        if self.sh.visible.swap(on, Ordering::SeqCst) == on {
            return;
        }
        self.sh.snap_gen.fetch_add(1, Ordering::SeqCst);
        if on {
            self.sh.shown.fetch_add(1, Ordering::SeqCst);
        } else {
            let _ = self.sh.request(&format!("lobby-closed\t{}", self.sh.window_id()));
        }
        self.refresh();
    }

    /// 退出 in the tray: offline until the lobby is opened again. The
    /// service stops too (it starts again with the lobby, or with Windows),
    /// so nothing runs the exe any more.
    pub fn quit(&self) {
        self.flush_undoables();
        let remote = matches!(*self.sh.mode.lock().unwrap(), Mode::Remote);
        self.goodbye(net::CLOSE_STOPPED);
        if remote {
            *self.sh.busy.lock().unwrap() = Some(crate::i18n::tr("正在退出", "Quitting…").into());
            match install::stop_service_now() {
                Ok(()) => log!("lobby: quit: the service stopped"),
                Err(e) => log!("lobby: quit: stopping the service: {e:#}"),
            }
        }
        crate::goaway::done();
        std::process::exit(0);
    }

    pub fn prepare_for_self_update(&self) {
        self.flush_undoables();
        self.goodbye(net::CLOSE_STOPPED);
        self.sh.stop_local();
    }

    /// The window closes: sessions hosted here end with `code` (the
    /// service's go on). The service forgets the temporary devices now
    /// instead of when the snapshots stop coming.
    pub fn goodbye(&self, code: u32) {
        let node = match &*self.sh.mode.lock().unwrap() {
            Mode::Local(n) => Some(n.clone()),
            Mode::Remote => {
                let _ = crate::pipe::call(node::PIPE, &format!("lobby-closed\t{}", self.sh.window_id()), Duration::from_secs(1));
                None
            }
            Mode::Off => None,
        };
        if let Some(n) = node {
            self.sh.rt.block_on(n.goodbye(code));
        }
    }
}

/// Whether the service runs, starting it first if it is installed but
/// stopped (it was not set to start with the computer).
fn service_up() -> bool {
    match install::service_running() {
        Some(true) => true,
        Some(false) => {
            if let Err(e) = install::start_service() {
                log!("lobby: starting the service: {e:#}");
                return false;
            }
            log!("lobby: started the service");
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(5) {
                if install::service_running() == Some(true) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        }
        None => false,
    }
}

fn my_session() -> u32 {
    let mut s = 0u32;
    unsafe {
        let _ = windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
            windows::Win32::System::Threading::GetCurrentProcessId(),
            &mut s,
        );
    }
    s
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Runs `view` with a session key of its own and the node's proof for it,
/// following its `@status` lines. Errors are what the user is told.
fn session(sh: &Arc<Sh>, id: EndpointId, name: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let dial = sh.request(&format!("dial\t{id}"))?;
    let f: Vec<&str> = dial.split('\t').collect();
    let list = |i: usize| -> Vec<String> {
        f.get(i).map_or(Vec::new(), |s| s.split(',').filter(|x| !x.is_empty()).map(String::from).collect())
    };
    let (addrs, via) = (list(0), list(1));
    let own = f.get(2).copied().unwrap_or("");
    let public = f.get(3) != Some(&"0");
    let session_key = SecretKey::generate();
    let proof = sh.request(&format!("proof\t{}", session_key.public()))?;
    let mut args: Vec<String> = vec!["view".into(), id.to_string(), "--name".into(), name.into(), "--proof-stdin".into()];
    for a in &addrs {
        args.extend(["--addr".into(), a.clone()]);
    }
    for v in &via {
        args.extend(["--via".into(), v.clone()]);
    }
    for r in own.split(',').filter(|r| !r.is_empty()) {
        args.extend(["--relay".into(), r.into()]);
    }
    if !public {
        args.push("--no-public".into());
    }
    args.extend(sh.view_args.iter().cloned());
    let log_path = sh.key.with_file_name(format!("session-{}.log", id.fmt_short()));
    log!("lobby: session to {name}: {}", args.join(" "));
    if sh.sessions.lock().unwrap().get(&id).is_some_and(|e| e.cancelled) {
        return Ok(());
    }
    let mut log_file = std::fs::File::create(&log_path)?;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(&args);
    if !sh.view_args.iter().any(|x| x == "--key" || x.starts_with("--key=")) {
        cmd.arg("--key").arg(&sh.key);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(log_file.try_clone()?)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    {
        let mut s = sh.sessions.lock().unwrap();
        if let Some(e) = s.get_mut(&id) {
            if e.cancelled {
                drop(s);
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            e.child_pid = Some(child.id());
        }
    }
    {
        let mut stdin = child.stdin.take().expect("piped");
        writeln!(stdin, "{}\n{proof}", hex(&session_key.to_bytes()))?;
    }
    let out = child.stdout.take().expect("piped");
    let t = Instant::now();
    let mut ending: Option<Result<(), String>> = None;
    for line in std::io::BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        let _ = writeln!(log_file, "{line}");
        let Some(st) = line.strip_prefix("@status ") else { continue };
        let (word, text) = st.split_once('\t').unwrap_or((st, ""));
        let state = match word {
            "connecting" => Some(SessionState::Connecting),
            "waiting" => Some(SessionState::Waiting),
            "connected" | "resumed" => Some(SessionState::Connected),
            "paused" => Some(SessionState::Paused),
            "failed" => {
                ending = Some(Err(text.to_string()));
                None
            }
            "closed" => {
                ending = Some(Ok(()));
                if !text.is_empty() {
                    let msg = if crate::i18n::is_en() {
                        format!("Disconnected from \"{name}\": {text}")
                    } else {
                        format!("已和「{name}」断开：{text}")
                    };
                    sh.note(Tone::Warn, msg);
                }
                None
            }
            _ => None,
        };
        if let Some(s) = state {
            if let Some(e) = sh.sessions.lock().unwrap().get_mut(&id) {
                e.state = s;
                e.since = Instant::now();
            }
            sh.changed();
        }
    }
    let status = child.wait()?;
    log!("lobby: session to {name} ended after {:.1} s ({status})", t.elapsed().as_secs_f64());
    if sh.sessions.lock().unwrap().get(&id).is_some_and(|e| e.cancelled) {
        return Ok(());
    }
    match ending {
        Some(Err(e)) => Err(anyhow!("{e}")),
        None if !status.success() => Err(anyhow!(
            "{}",
            if crate::i18n::is_en() {
                format!("Session window exited unexpectedly ({status}); see {}", log_path.display())
            } else {
                format!("会话窗口异常退出（{status}），详情见 {}", log_path.display())
            }
        )),
        _ => Ok(()),
    }
}

/// Runs install-service / uninstall-service elevated (one UAC prompt) and
/// returns what to tell the user.
fn run_elevated(install: bool, key: &Path) -> Result<String> {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
            UI::{
                Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
                WindowsAndMessaging::SW_HIDE,
            },
        },
        core::{HSTRING, w},
    };
    let report = key.with_file_name("service-setup.log");
    let _ = std::fs::remove_file(&report);
    let exe = std::env::current_exe()?;
    let params = format!(
        "{} --from-lobby \"{}\" --lobby-pid {}",
        if install { "install-service" } else { "uninstall-service" },
        key.display(),
        std::process::id()
    );
    let (file, params) = (HSTRING::from(exe.as_os_str()), HSTRING::from(params));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    let code = unsafe {
        ShellExecuteExW(&mut info).map_err(|e: windows::core::Error| {
            if e.code().0 as u32 == 0x8007_04C7 {
                anyhow!("{}", crate::i18n::tr("管理员确认被取消了", "Administrator prompt was cancelled"))
            } else {
                anyhow!("{e}")
            }
        })?;
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code
    };
    let out = std::fs::read_to_string(&report).unwrap_or_default();
    log!("{} exit {code}:\n{out}", if install { "install-service" } else { "uninstall-service" });
    if code != 0 {
        let why = out
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("Error: "))
            .unwrap_or_else(|| crate::i18n::tr("没有输出", "no output"));
        return Err(anyhow!("{why}"));
    }
    Ok(if install {
        crate::i18n::tr("后台服务已装好", "Background service installed").into()
    } else {
        crate::i18n::tr("后台服务已卸载", "Background service uninstalled").into()
    })
}
