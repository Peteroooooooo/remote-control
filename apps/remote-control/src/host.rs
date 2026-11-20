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
        self.quality.unwrap_or(Quality { fit_w: 0, fit_h: 0, short: 0, fps: self.fps, kbps: self.max_kbps, yuv444: false })
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
    // The capture loop sets the real rate and highest bitrate before the first frame.
    let tx = VideoTx::start(conn.clone(), 60, 20e6);
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
    let sh = Arc::new(PipeShared::default());
    *sh.quality.lock().unwrap() = hello.quality();
    let pipe = {
        let (tx, sh, info) = (tx.clone(), sh.clone(), wtx.clone());
        std::thread::Builder::new().name("capture".into()).spawn(move || pipeline::run(cfg, tx, sh, info))?
    };
    {
        let (out, sh) = (wtx.clone(), sh.clone());
        std::thread::Builder::new().name("cursor".into()).spawn(move || cursor::watch(out, sh))?;
    }
    let clip = match ClipSync::start(conn, wtx.clone(), true, service, None) {
        Ok(c) => Some(c),
        Err(e) => {
            log!("clipboard sharing off: {e:#}");
            None
        }
    };
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
    let audio_host = AudioHost::start(conn.clone(), tokio::runtime::Handle::current(), audio_paused.clone());
    tokio::spawn(watch_path(conn.clone(), tx.clone(), audio_paused, sh.clone(), wtx.clone()));
    tokio::spawn(watch_hint(conn.clone(), sh.clone(), wtx.clone()));
    let result = control_loop(recv, &sh, &wtx, clip.as_deref(), &audio_host).await;
    drop(audio_host);
    sh.stop.store(true, Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || pipe.join()).await;
    conn.close(0u32.into(), b"bye");
    result
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

/// Applies the viewer's input until the control stream ends.
async fn control_loop(
    recv: &mut RecvStream,
    sh: &PipeShared,
    wtx: &mpsc::UnboundedSender<Msg>,
    clip: Option<&ClipSync>,
    audio_host: &AudioHost,
) -> Result<()> {
    let mut inj = Injector::new(*sh.display.lock().unwrap());
    let result = loop {
        let msg = match read_msg(recv).await {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        inj.set_display(*sh.display.lock().unwrap());
        match msg {
            Msg::MouseMove { x, y } => inj.move_to(x, y),
            Msg::MouseButton { button, down } => inj.button(button, down),
            Msg::Wheel { delta, horizontal } => inj.wheel(delta, horizontal),
            Msg::Key { scan, down } => inj.key(scan, down),
            Msg::ReleaseAll => inj.release_all(),
            Msg::RequestKeyframe => sh.want_key.store(true, Ordering::Relaxed),
            Msg::Quality(q) => {
                let mut cur = sh.quality.lock().unwrap();
                if *cur != q {
                    log!("viewer wants the picture {q:?}");
                    *cur = q;
                    sh.quality_changed.store(true, Ordering::Relaxed);
                }
            }
            Msg::SelectMonitor { index } => match crate::monitors::select(&sh.monitors, index) {
                // The capture loop reopens and sends the new list itself.
                Ok(true) => log!("viewer picked display {}", index as u32 + 1),
                r => {
                    if let Err(e) = r {
                        log!("{e:#}");
                    }
                    let _ = wtx.send(crate::monitors::message(&sh.monitors));
                }
            },
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
                inj.move_to(x, y);
                tokio::time::sleep(Duration::from_millis(50)).await;
                let (cx, cy) = inj.cursor();
                let _ = wtx.send(Msg::ProbeReply { x: cx, y: cy });
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
    inj.release_all();
    // The viewer closing its window ends the stream; that is a normal exit.
    match result {
        Err(e) if format!("{e:#}").contains("closed") => Ok(()),
        r => r,
    }
}
