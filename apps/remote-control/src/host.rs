//! The host side of a desktop session: streams one display (the primary,
//! until the viewer picks another) and applies the viewer's input. Who may
//! start a session is decided before, by the node (`node::admit`).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use clap::Args;
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use tokio::sync::mpsc;

use crate::{
    audio::agent::AudioHost,
    clipsync::ClipSync,
    cursor,
    encode::EncOptions,
    inject::Injector,
    log,
    mf::Codec,
    net,
    pipeline::{self, PipeConfig, PipeShared},
    proto::{CAN_HEVC, CAN_HEVC444, HINT_NO_DISPLAY, HINT_NO_SAS, HINT_NONE, Msg, Quality, read_msg},
    video_tx::VideoTx,
};

#[derive(Args)]
pub struct HostArgs {
    /// Device id that may control this computer without asking (repeatable,
    /// for tests): put in the device list as "my computer".
    #[arg(long)]
    pub allow: Vec<EndpointId>,
    /// Highest frame rate to send; the viewer chooses below it (and below the
    /// display's refresh rate).
    #[arg(long, default_value_t = 240)]
    fps: u32,
    /// Highest video bitrate in Mbit/s; default: what the viewer chose.
    #[arg(long)]
    mbps: Option<f64>,
    /// Scale the picture down to this width before encoding (0 = native).
    #[arg(long, default_value_t = 0)]
    max_width: u32,
    /// Encoder quality-versus-speed, 0 (fastest) to 100 (best picture).
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=100))]
    speed: Option<u32>,
    /// Encoder rate buffer, in frames' worth of bits (smaller = steadier frame
    /// sizes); default 0.75 on Intel Quick Sync, 1 elsewhere. Intel's encoders
    /// die in sessions at 0.5 (P1-2).
    #[arg(long)]
    vbv: Option<f64>,
    /// `auto`: HEVC when this computer has a usable hardware HEVC encoder (not
    /// Intel's yet) and the viewer can decode it, else H.264.
    #[arg(long, default_value = "auto", value_parser = ["auto", "h264"])]
    codec: String,
    /// Leave out the desktop-streaming hints to the encoder (for comparison).
    #[arg(long)]
    no_hints: bool,
    /// Lowest pacing rate, as a multiple of the chosen video rate; 0 = plain
    /// BBR, which slows to a crawl after a still screen (for comparison).
    #[arg(long, default_value_t = 4.0)]
    pub floor_pacing: f64,
    /// When frames queue up, ask the encoder for a smaller frame instead of
    /// skipping one. The NVIDIA and Intel encoders tested ignore such
    /// single-frame requests, so this is off (for comparison).
    #[arg(long)]
    shrink: bool,
    /// Device key file (default: %APPDATA%\remote-control\device.key).
    #[arg(long)]
    pub key: Option<PathBuf>,
    /// Do not answer LAN discovery (other lobbies on the network will not list this computer).
    #[arg(long)]
    pub no_lan: bool,
}

impl HostArgs {
    /// The defaults of `remote-control host` (for the lobby).
    pub fn defaults() -> HostArgs {
        #[derive(clap::Parser)]
        struct P {
            #[command(flatten)]
            a: HostArgs,
        }
        <P as clap::Parser>::parse_from(["host"]).a
    }

    /// The node's options (and the pacing floor, which is process-wide).
    pub fn node_options(&self, service: bool) -> crate::node::Options {
        crate::cc::set_pacing_gain(self.floor_pacing);
        crate::node::Options {
            key: self.key.clone().unwrap_or_else(net::default_key_path),
            service,
            allow: self.allow.clone(),
            lan: !self.no_lan,
            pipe: self.pipe_config(),
        }
    }

    pub fn pipe_config(&self) -> PipeConfig {
        PipeConfig {
            max_fps: self.fps.clamp(1, 240),
            max_bps: self.mbps.map(|m| (m.clamp(1.0, 1000.0) * 1e6) as u32),
            max_width: self.max_width,
            enc: EncOptions {
                codec: Codec::H264,
                speed: self.speed,
                vbv_frames: self.vbv.map(|v| v.clamp(0.25, 120.0)),
                layers_hint: !self.no_hints,
                scenario_hint: !self.no_hints,
                qp_control: false,
                yuv444: false,
                allow_untrusted_hevc: false,
            },
            shrink: self.shrink,
            hevc: self.codec == "auto",
            viewer444: false,
        }
    }
}

/// Desktop sessions in progress, for `close_sessions`.
static SESSIONS: std::sync::Mutex<Vec<Connection>> = std::sync::Mutex::new(Vec::new());

/// Closes every desktop session with `code` (`net::CLOSE_*`), so viewers
/// learn why at once instead of at the idle timeout.
pub fn close_sessions(code: u32) {
    let reason: &[u8] = match code {
        net::CLOSE_SHUTDOWN => b"restarting or shutting down",
        net::CLOSE_DISCONNECTED => b"disconnected on the host",
        _ => b"host stopped",
    };
    for c in SESSIONS.lock().unwrap().drain(..) {
        c.close(code.into(), reason);
    }
}

pub fn session_count() -> usize {
    SESSIONS.lock().unwrap().len()
}

/// A shell, command or copy (`term`) counts as a session: 【断开】 and going away end it too.
pub fn track(conn: &Connection) {
    SESSIONS.lock().unwrap().push(conn.clone());
}

pub fn untrack(conn: &Connection) {
    let id = conn.stable_id();
    SESSIONS.lock().unwrap().retain(|c| c.stable_id() != id);
}

/// The viewer's first message, read by the core before admitting it.
pub struct Hello {
    pub max_kbps: u32,
    pub fps: u8,
    pub caps: u8,
    /// None from viewers before the 画面 menu: then `max_kbps` and `fps` hold.
    pub quality: Option<Quality>,
}

impl Hello {
    /// What the viewer wants the picture to be.
    fn quality(&self) -> Quality {
        self.quality.unwrap_or(Quality { fit_w: 0, fit_h: 0, short: 0, fps: self.fps, kbps: self.max_kbps, yuv444: false, auto_fps: false })
    }
}

/// Streams to an admitted viewer until either side ends the session.
/// `service`: this is the service's helper (files are read as the console user).
pub async fn session(conn: Connection, send: SendStream, mut recv: RecvStream, hello: Hello, cfg: PipeConfig, service: bool) -> Result<()> {
    SESSIONS.lock().unwrap().push(conn.clone());
    let id = conn.stable_id();
    let r = stream(&conn, send, &mut recv, hello, cfg, service).await;
    SESSIONS.lock().unwrap().retain(|c| c.stable_id() != id);
    r
}

async fn stream(conn: &Connection, mut send: SendStream, recv: &mut RecvStream, hello: Hello, cfg: PipeConfig, service: bool) -> Result<()> {
    let hevc = cfg.hevc && hello.caps & CAN_HEVC != 0;
    let cfg = PipeConfig { hevc, viewer444: hevc && hello.caps & CAN_HEVC444 != 0, ..cfg };
    let sh = Arc::new(PipeShared::default());
    *sh.quality.lock().unwrap() = hello.quality();
    // The capture loop sets the real rate and highest bitrate before the first frame.
    let tx = VideoTx::start(conn.clone(), 60, 20e6, sh.want_key.clone());
    let _ = send.set_priority(100);
    let (wtx, mut wrx) = mpsc::unbounded_channel::<Msg>();
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(m) = wrx.recv().await {
                if send.write_all(&m.encode()).await.is_err() {
                    break;
                }
                tx.notify_ctrl();
            }
        });
    }
    let pipe = {
        let (tx, sh, info) = (tx.clone(), sh.clone(), wtx.clone());
        std::thread::Builder::new().name("capture".into()).spawn(move || pipeline::run(cfg, tx, sh, info))?
    };
    {
        let (out, sh) = (wtx.clone(), sh.clone());
        std::thread::Builder::new().name("cursor".into()).spawn(move || cursor::watch(out, sh))?;
    }
    let win_watch = {
        let (out, sh) = (wtx.clone(), sh.clone());
        std::thread::Builder::new().name("windows".into()).spawn(move || watch_windows(out, sh))?
    };
    let clip = match ClipSync::start_clip(conn, wtx.clone(), true, service) {
        Ok(c) => Some(c),
        Err(e) => {
            log!("clipboard sharing off: {e:#}");
            None
        }
    };
    tokio::spawn(crate::clipsync::accept_uni_loop(conn.clone(), clip.as_ref().map(Arc::downgrade), None));
    {
        let (tx, wtx, conn) = (tx.clone(), wtx.clone(), conn.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            while conn.close_reason().is_none() {
                tick.tick().await;
                if wtx.send(Msg::Stats(tx.take_stats())).is_err() {
                    break;
                }
            }
        });
    }
    let audio_paused = Arc::new(AtomicBool::new(false));
    let audio_host = AudioHost::start(
        conn.clone(),
        tokio::runtime::Handle::current(),
        audio_paused.clone(),
        tx.audio_red_level(),
    );
    tokio::spawn(watch_path(conn.clone(), tx.clone(), audio_paused, sh.clone(), wtx.clone()));
    tokio::spawn(watch_hint(conn.clone(), sh.clone(), wtx.clone()));
    let result = control_loop(recv, &sh, &wtx, clip.as_deref(), &audio_host).await;
    let _ = tokio::task::spawn_blocking(move || drop(audio_host)).await;
    sh.stop.store(true, Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = pipe.join();
        let _ = win_watch.join();
    }).await;
    conn.close(0u32.into(), b"bye");
    result
}

/// Polls real visible top-level windows on a dedicated thread (so `SetThreadDesktop`
/// never leaks onto Tokio worker threads), pushes `Msg::Windows` on change,
/// real Windows process icons (`Msg::AppIcon`) once per app_key, and live
/// window thumbnails (`Msg::WindowThumb`) whenever a window's image changes.
fn watch_windows(out: mpsc::UnboundedSender<Msg>, sh: Arc<PipeShared>) {
    use std::collections::{HashMap, HashSet};

    let mut last: Option<Vec<crate::proto::WindowDesc>> = None;
    let mut sent_icons: HashSet<String> = HashSet::new();
    let mut thumb_hashes: HashMap<u64, u64> = HashMap::new();
    let mut waited_for_monitors = 0u32;
    let mut cycle: u32 = 0;

    fn fnv64(data: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    while !sh.stop.load(Ordering::Relaxed) {
        let mons = crate::monitors::snapshot(&sh.monitors);
        if mons.is_empty() && waited_for_monitors < 10 {
            waited_for_monitors += 1;
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        let list = crate::winlist::enumerate(&mons);
        let list_changed = last.as_ref() != Some(&list);
        if list_changed {
            last = Some(list.clone());
            if out.send(Msg::Windows { list: list.clone() }).is_err() {
                return;
            }
        }

        // Send real Windows process/window icon PNG once per app_key.
        for win in &list {
            if sh.stop.load(Ordering::Relaxed) {
                return;
            }
            if !sent_icons.contains(&win.app_key) {
                if let Some(png) = crate::winlist::extract_app_icon_png(win.hwnd) {
                    sent_icons.insert(win.app_key.clone());
                    if out.send(Msg::AppIcon { app_key: win.app_key.clone(), png }).is_err() {
                        return;
                    }
                }
            }
        }

        // Capture real live window thumbnails on first sight, list change, or every 2nd cycle (~2.5 s).
        // Skip while in single-window or virtual-display fullscreen mode to keep zero extra CPU load.
        let in_focused_stream =
            sh.target_window.load(Ordering::Relaxed) != 0 || crate::monitors::is_virtual_active(&sh.monitors);
        if !in_focused_stream && (list_changed || cycle % 2 == 0) {
            let live_hwnds: HashSet<u64> = list.iter().map(|w| w.hwnd).collect();
            thumb_hashes.retain(|h, _| live_hwnds.contains(h));
            for win in list.iter().take(24) {
                if sh.stop.load(Ordering::Relaxed)
                    || sh.target_window.load(Ordering::Relaxed) != 0
                    || crate::monitors::is_virtual_active(&sh.monitors)
                {
                    break;
                }
                if win.minimized {
                    continue;
                }
                if let Some(png) = crate::winlist::capture_window_thumb_png(win.hwnd) {
                    let h = fnv64(&png);
                    if thumb_hashes.get(&win.hwnd) != Some(&h) {
                        thumb_hashes.insert(win.hwnd, h);
                        if out.send(Msg::WindowThumb { hwnd: win.hwnd, png }).is_err() {
                            return;
                        }
                    }
                }
            }
        }

        cycle = cycle.wrapping_add(1);
        for _ in 0..12 {
            if sh.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// How long the picture may stay paused before the session ends.
const PAUSE_LIMIT: Duration = Duration::from_secs(10);

/// Video and input never go through a public relay: while the only way left
/// is one, the picture pauses; if no direct way (or own relay) comes back
/// within `PAUSE_LIMIT`, the session ends.
async fn watch_path(
    conn: Connection,
    tx: Arc<VideoTx>,
    audio_paused: Arc<AtomicBool>,
    sh: Arc<PipeShared>,
    wtx: mpsc::UnboundedSender<Msg>,
) {
    let mut since: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    while conn.close_reason().is_none() {
        tick.tick().await;
        let (kind, addr, _) = net::selected_path(&conn);
        let public = kind == net::PathKind::Public;
        match (public, since) {
            (true, None) => {
                log!("only a public relay ({addr}) between us: picture paused");
                since = Some(Instant::now());
                tx.paused.store(true, Ordering::Relaxed);
                audio_paused.store(true, Ordering::Relaxed);
                let _ = wtx.send(Msg::Paused { on: true });
            }
            (true, Some(t)) if t.elapsed() > PAUSE_LIMIT => {
                log!("no direct way back within {} s: ending the session", PAUSE_LIMIT.as_secs());
                conn.close(net::CLOSE_NO_DIRECT.into(), b"no direct path");
                return;
            }
            (false, Some(t)) => {
                log!("{} again after {:.1} s: picture goes on", kind.label(), t.elapsed().as_secs_f64());
                since = None;
                tx.paused.store(false, Ordering::Relaxed);
                audio_paused.store(false, Ordering::Relaxed);
                sh.want_key.store(true, Ordering::Relaxed);
                let _ = wtx.send(Msg::Paused { on: false });
            }
            _ => {}
        }
    }
}

/// Tells the viewer why no picture comes (`Msg::Hint`), and when it comes
/// again. The session stays: the capture loop keeps trying and the picture
/// comes back by itself.
async fn watch_hint(conn: Connection, sh: Arc<PipeShared>, wtx: mpsc::UnboundedSender<Msg>) {
    let mut last = HINT_NONE;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    while conn.close_reason().is_none() {
        tick.tick().await;
        let hint = if sh.no_display.load(Ordering::Relaxed) {
            HINT_NO_DISPLAY
        } else {
            tokio::task::spawn_blocking(crate::desktop::secure_screen_hint).await.unwrap_or(HINT_NONE)
        };
        if hint != last {
            log!("hint to the viewer: {hint} (was {last})");
            last = hint;
            if hint == HINT_NONE {
                sh.want_key.store(true, Ordering::Relaxed);
            }
            if wtx.send(Msg::Hint { what: hint }).is_err() {
                return;
            }
        }
    }
}

enum InjectCmd {
    Move { display: windows::Win32::Foundation::RECT, x: u16, y: u16 },
    MouseDelta { display: windows::Win32::Foundation::RECT, dx: i16, dy: i16 },
    Button { display: windows::Win32::Foundation::RECT, button: u8, down: bool },
    Touch { display: windows::Win32::Foundation::RECT, id: u8, phase: u8, x: u16, y: u16 },
    Touchpad { display: windows::Win32::Foundation::RECT, phase: u8, dx: i16, dy: i16, scale_milli: u16 },
    Pen { display: windows::Win32::Foundation::RECT, phase: u8, x: u16, y: u16, pressure: u16, tilt_x: i8, tilt_y: i8, flags: u8 },
    Wheel { display: windows::Win32::Foundation::RECT, delta: i16, horizontal: bool },
    Key { display: windows::Win32::Foundation::RECT, scan: u16, down: bool },
    Text { display: windows::Win32::Foundation::RECT, text: String },
    FocusWindow {
        hwnd: u64,
        monitors: Vec<crate::proto::MonitorDesc>,
        reply: tokio::sync::oneshot::Sender<Option<(u8, Vec<crate::proto::WindowDesc>)>>,
    },
    ConfigureVirtualDisplay {
        active: bool,
        width: u16,
        height: u16,
        dpi_percent: u16,
        reply: tokio::sync::oneshot::Sender<Option<crate::vdisplay::ActiveVirtualDisplay>>,
    },
    RestoreWindow {
        destroy_vdisplay: bool,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    ReleaseAll { display: windows::Win32::Foundation::RECT },
    CheckTextFocus { wtx: mpsc::UnboundedSender<Msg> },
    Cursor { display: windows::Win32::Foundation::RECT, reply: tokio::sync::oneshot::Sender<(u16, u16)> },
}

/// Applies the viewer's input until the control stream ends.
/// All `SetThreadDesktop` / `SendInput` / `GetCursorPos` / virtual-display
/// lifecycle calls are isolated to a dedicated OS thread so Tokio worker threads
/// never have their desktop binding mutated or blocked, and window restore
/// always executes strictly before virtual-display teardown.
async fn control_loop(
    recv: &mut RecvStream,
    sh: &PipeShared,
    wtx: &mpsc::UnboundedSender<Msg>,
    clip: Option<&ClipSync>,
    audio_host: &AudioHost,
) -> Result<()> {
    let init_display = *sh.display.lock().unwrap();
    let (inj_tx, inj_rx) = std::sync::mpsc::channel::<InjectCmd>();
    let inj_thread = std::thread::Builder::new().name("inject".into()).spawn(move || {
        let mut inj = Injector::new(init_display);
        let mut vdisplay = crate::vdisplay::VirtualDisplayManager::new();
        let mut saved_win: Option<crate::winlist::SavedWindow> = None;
        let mut last_move_pos: Option<(u16, u16)> = None;
        let mut last_move_at = Instant::now() - Duration::from_secs(1);
        let mut caret_before: Option<(isize, i32, i32)> = None;
        loop {
            // While a finger is held still, wake up to keep the touch contact alive.
            let cmd = match inj.touch_keepalive_wait() {
                Some(wait) => match inj_rx.recv_timeout(wait) {
                    Ok(cmd) => cmd,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        inj.touch_keepalive();
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match inj_rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => break,
                },
            };
            match cmd {
                InjectCmd::Move { display, x, y } => {
                    inj.set_display(display);
                    let moved = last_move_pos.is_none_or(|(px, py)| px.abs_diff(x) > 8 || py.abs_diff(y) > 8);
                    inj.move_to(x, y);
                    last_move_pos = Some((x, y));
                    if moved {
                        last_move_at = Instant::now();
                    }
                }
                InjectCmd::MouseDelta { display, dx, dy } => {
                    inj.set_display(display);
                    inj.move_delta(dx, dy);
                }
                InjectCmd::Button { display, button, down } => {
                    inj.set_display(display);
                    if button == 0 && down {
                        if last_move_at.elapsed() < Duration::from_millis(8) {
                            std::thread::sleep(Duration::from_millis(6));
                        }
                        caret_before = crate::cursor::caret_snapshot();
                    }
                    inj.button(button, down);
                }
                InjectCmd::Touch { display, id, phase, x, y } => {
                    inj.set_display(display);
                    if id == 0 && phase == 0 {
                        caret_before = crate::cursor::caret_snapshot();
                    }
                    inj.touch(id, phase, x, y);
                }
                InjectCmd::Touchpad { display, phase, dx, dy, scale_milli } => {
                    inj.set_display(display);
                    inj.touchpad(phase, dx, dy, scale_milli);
                }
                InjectCmd::Pen { display, phase, x, y, pressure, tilt_x, tilt_y, flags } => {
                    inj.set_display(display);
                    inj.pen(phase, x, y, pressure, tilt_x, tilt_y, flags);
                }
                InjectCmd::Wheel { display, delta, horizontal } => {
                    inj.set_display(display);
                    inj.wheel(delta, horizontal);
                }
                InjectCmd::Key { display, scan, down } => {
                    inj.set_display(display);
                    inj.key(scan, down);
                }
                InjectCmd::Text { display, text } => {
                    inj.set_display(display);
                    inj.text(&text);
                }
                InjectCmd::FocusWindow { hwnd, monitors, reply } => {
                    if let Some(prev) = saved_win.take() {
                        crate::winlist::restore_placement(prev);
                    }
                    let res = crate::winlist::focus_window_only(hwnd, &monitors)
                        .map(|mon_idx| (mon_idx, crate::winlist::enumerate(&monitors)));
                    let _ = reply.send(res);
                }
                InjectCmd::ConfigureVirtualDisplay { active, width, height, dpi_percent, reply } => {
                    if !active {
                        if let Some(prev) = saved_win.take() {
                            crate::winlist::restore_placement(prev);
                        }
                        vdisplay.destroy();
                        let _ = reply.send(None);
                        continue;
                    }
                    if !vdisplay.is_supported() {
                        let _ = reply.send(None);
                        continue;
                    }
                    match vdisplay.ensure(width, height, dpi_percent) {
                        Ok(vd) => {
                            if let Some(saved) = saved_win {
                                let _ = crate::winlist::teleport_window_to_monitor(saved.hwnd, vd.rect);
                            }
                            let _ = reply.send(Some(vd));
                        }
                        Err(e) => {
                            log!("vdisplay: configure failed: {e:#}");
                            let _ = reply.send(None);
                        }
                    }
                }
                InjectCmd::RestoreWindow { destroy_vdisplay, reply } => {
                    if let Some(prev) = saved_win.take() {
                        crate::winlist::restore_placement(prev);
                    }
                    let mut destroyed = false;
                    if destroy_vdisplay && vdisplay.active().is_some() {
                        vdisplay.destroy();
                        destroyed = true;
                    }
                    let _ = reply.send(destroyed);
                }
                InjectCmd::ReleaseAll { display } => {
                    inj.set_display(display);
                    inj.release_all();
                }
                InjectCmd::CheckTextFocus { wtx } => {
                    let after = crate::cursor::caret_snapshot();
                    let caret_activated = after.is_some() && after != caret_before;
                    if caret_activated || crate::cursor::is_uia_editable_now() {
                        let _ = wtx.send(Msg::TextFocus { editable: true });
                    }
                }
                InjectCmd::Cursor { display, reply } => {
                    inj.set_display(display);
                    let _ = reply.send(inj.cursor());
                }
            }
        }
        // Strictly restore window placement BEFORE destroying the virtual display.
        if let Some(prev) = saved_win.take() {
            crate::winlist::restore_placement(prev);
        }
        inj.release_all();
        vdisplay.destroy();
    })?;
    let guard_epoch = Instant::now();
    let mut input_guard = crate::input_guard::InputGuard::new();
    let mut batch_verdict = crate::input_guard::InputVerdict::Accept;
    let result = loop {
        let msg = match read_msg(recv).await {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        let display = *sh.display.lock().unwrap();
        match msg {
            Msg::InputTick { ms } => {
                let recv_ms = guard_epoch.elapsed().as_millis() as u64;
                batch_verdict = input_guard.check(ms, recv_ms);
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Drop { release_all: true }) {
                    log!("stale input (>2 s late): dropping buffered events and releasing held keys");
                    let _ = inj_tx.send(InjectCmd::ReleaseAll { display });
                }
            }
            Msg::MouseMove { x, y } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Move { display, x, y });
                }
            }
            Msg::MouseDelta { dx, dy } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::MouseDelta { display, dx, dy });
                }
            }
            Msg::MouseButton { button, down } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Button { display, button, down });
                    if button == 0 && !down {
                        let tx = inj_tx.clone();
                        let wtx_clone = wtx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(65)).await;
                            let _ = tx.send(InjectCmd::CheckTextFocus { wtx: wtx_clone });
                        });
                    }
                }
            }
            Msg::Touch { id, phase, x, y } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Touch { display, id, phase, x, y });
                    if id == 0 && phase == 2 {
                        let tx = inj_tx.clone();
                        let wtx_clone = wtx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(70)).await;
                            let _ = tx.send(InjectCmd::CheckTextFocus { wtx: wtx_clone });
                        });
                    }
                }
            }
            Msg::Touchpad { phase, dx, dy, scale_milli } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Touchpad { display, phase, dx, dy, scale_milli });
                }
            }
            Msg::Pen { phase, x, y, pressure, tilt_x, tilt_y, flags } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Pen { display, phase, x, y, pressure, tilt_x, tilt_y, flags });
                }
            }
            Msg::Wheel { delta, horizontal } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Wheel { display, delta, horizontal });
                }
            }
            Msg::Key { scan, down } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) {
                    let _ = inj_tx.send(InjectCmd::Key { display, scan, down });
                }
            }
            Msg::TextInput { text } => {
                if matches!(batch_verdict, crate::input_guard::InputVerdict::Accept) && !text.is_empty() {
                    let _ = inj_tx.send(InjectCmd::Text { display, text });
                }
            }
            Msg::FocusWindow { hwnd, .. } => {
                if hwnd == 0 {
                    crate::monitors::set_vdisplay_transitioning(&sh.monitors, true);
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let destroyed_vd = if inj_tx
                        .send(InjectCmd::RestoreWindow { destroy_vdisplay: true, reply: reply_tx })
                        .is_ok()
                    {
                        reply_rx.await.unwrap_or(false)
                    } else {
                        false
                    };
                    let cleared_vd = crate::monitors::clear_virtual(&sh.monitors);
                    if sh.target_window.swap(0, Ordering::AcqRel) != 0 || cleared_vd || destroyed_vd {
                        log!("viewer exited window/virtual mode: returning to physical display");
                        sh.target_window_changed.store(true, Ordering::Release);
                    }
                    if cleared_vd || destroyed_vd {
                        let _ = wtx.send(Msg::VirtualDisplay {
                            active: false,
                            width: 0,
                            height: 0,
                            dpi_percent: 0,
                        });
                    }
                } else {
                    let monitors = crate::monitors::snapshot(&sh.monitors);
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    if inj_tx
                        .send(InjectCmd::FocusWindow { hwnd, monitors, reply: reply_tx })
                        .is_ok()
                        && let Ok(Some((mon_idx, list))) = reply_rx.await
                    {
                        if sh.target_window.swap(0, Ordering::AcqRel) != 0 {
                            sh.target_window_changed.store(true, Ordering::Release);
                        }
                        match crate::monitors::select(&sh.monitors, mon_idx) {
                            Ok(true) => {
                                log!(
                                    "viewer focused window 0x{hwnd:x} on physical display {} (switching monitor)",
                                    mon_idx as u32 + 1
                                );
                            }
                            Ok(false) => {
                                let _ = wtx.send(crate::monitors::message(&sh.monitors));
                                log!(
                                    "viewer focused window 0x{hwnd:x} on physical display {}",
                                    mon_idx as u32 + 1
                                );
                            }
                            Err(e) => {
                                log!(
                                    "focus window 0x{hwnd:x}: failed to select display {}: {e:#}",
                                    mon_idx as u32 + 1
                                );
                            }
                        }
                        let _ = wtx.send(Msg::Windows { list });
                    }
                }
            }
            Msg::VirtualDisplay { active, width, height, dpi_percent } => {
                crate::monitors::set_vdisplay_transitioning(&sh.monitors, true);
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if inj_tx
                    .send(InjectCmd::ConfigureVirtualDisplay {
                        active,
                        width,
                        height,
                        dpi_percent,
                        reply: reply_tx,
                    })
                    .is_ok()
                    && let Ok(vd_opt) = reply_rx.await
                {
                    match vd_opt {
                        Some(vd) => {
                            let reopen = crate::monitors::select_virtual(
                                &sh.monitors,
                                vd.device_name.clone(),
                                vd.width,
                                vd.height,
                            );
                            sh.target_window.store(0, Ordering::Release);
                            if reopen {
                                sh.target_window_changed.store(true, Ordering::Release);
                            }
                            let _ = wtx.send(Msg::VirtualDisplay {
                                active: true,
                                width: vd.width,
                                height: vd.height,
                                dpi_percent: vd.dpi_percent,
                            });
                            log!(
                                "virtual display active: {} ({}x{} @ {}%, reopen={reopen})",
                                vd.device_name,
                                vd.width,
                                vd.height,
                                vd.dpi_percent
                            );
                        }
                        None => {
                            let cleared = crate::monitors::clear_virtual(&sh.monitors);
                            if cleared {
                                sh.target_window_changed.store(true, Ordering::Release);
                            }
                            let _ = wtx.send(Msg::VirtualDisplay {
                                active: false,
                                width: 0,
                                height: 0,
                                dpi_percent: 0,
                            });
                        }
                    }
                } else {
                    crate::monitors::set_vdisplay_transitioning(&sh.monitors, false);
                }
            }
            Msg::ReleaseAll => {
                let _ = inj_tx.send(InjectCmd::ReleaseAll { display });
            }
            Msg::Throttle { fps } => {
                let prev = sh.throttle_fps.swap(fps, Ordering::Relaxed);
                if prev != fps {
                    if fps > 0 {
                        log!("viewer minimized: throttling capture to {fps} fps (keeping encoder)");
                    } else {
                        log!("viewer restored: resuming full frame rate");
                    }
                }
            }
            Msg::RequestKeyframe => sh.want_key.store(true, Ordering::Relaxed),
            Msg::Quality(q) => {
                let mut cur = sh.quality.lock().unwrap();
                if *cur != q {
                    log!("viewer wants the picture {q:?}");
                    *cur = q;
                    sh.quality_changed.store(true, Ordering::Release);
                }
            }
            Msg::SelectMonitor { index } => {
                crate::monitors::set_vdisplay_transitioning(&sh.monitors, true);
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let destroyed_vd = if inj_tx
                    .send(InjectCmd::RestoreWindow { destroy_vdisplay: true, reply: reply_tx })
                    .is_ok()
                {
                    reply_rx.await.unwrap_or(false)
                } else {
                    false
                };
                let cleared_vd = crate::monitors::clear_virtual(&sh.monitors);
                if sh.target_window.swap(0, Ordering::AcqRel) != 0 || cleared_vd || destroyed_vd {
                    sh.target_window_changed.store(true, Ordering::Release);
                }
                if cleared_vd || destroyed_vd {
                    let _ = wtx.send(Msg::VirtualDisplay {
                        active: false,
                        width: 0,
                        height: 0,
                        dpi_percent: 0,
                    });
                }
                match crate::monitors::select(&sh.monitors, index) {
                    // The capture loop reopens and sends the new list itself.
                    Ok(true) => log!("viewer picked display {}", index as u32 + 1),
                    r => {
                        if let Err(e) = r {
                            log!("{e:#}");
                        }
                        let _ = wtx.send(crate::monitors::message(&sh.monitors));
                    }
                }
            }
            Msg::SendSas => {
                let r = tokio::task::spawn_blocking(crate::service::request_sas).await;
                let sent = matches!(r, Ok(Ok(())));
                log!("Ctrl+Alt+Del: {}", match r {
                    Ok(Ok(())) => "sent".to_string(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => format!("{e}"),
                });
                if !sent {
                    let _ = wtx.send(Msg::Hint { what: HINT_NO_SAS });
                }
            }
            Msg::Ping { t_us } => {
                let _ = wtx.send(Msg::Pong { t_us });
            }
            Msg::Probe { x, y } => {
                let _ = inj_tx.send(InjectCmd::Move { display, x, y });
                tokio::time::sleep(Duration::from_millis(50)).await;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let cur_display = *sh.display.lock().unwrap();
                if inj_tx.send(InjectCmd::Cursor { display: cur_display, reply: reply_tx }).is_ok()
                    && let Ok((cx, cy)) = reply_rx.await
                {
                    let _ = wtx.send(Msg::ProbeReply { x: cx, y: cy });
                }
            }
            Msg::ClipOffer { seq, .. } => {
                if let Some(c) = clip {
                    c.on_offer(seq);
                }
            }
            Msg::Audio(cfg) => audio_host.configure(cfg),
            _ => {}
        }
    };
    drop(inj_tx);
    let _ = tokio::task::spawn_blocking(move || inj_thread.join()).await;
    // The viewer closing its window ends the stream; that is a normal exit.
    match result {
        Err(e) if format!("{e:#}").contains("closed") => Ok(()),
        r => r,
    }
}
