//! `remote-control view <id>`: connects to a host and shows its screen in a window.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use clap::Args;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
};
use tokio::{sync::mpsc, task::AbortHandle};
use windows::Win32::Foundation::HWND;

use crate::{
    clipsync::ClipSync,
    decode::{Decoder, Picture},
    gpu::Gpu,
    i18n::{is_en, tr},
    log,
    mf::{self, Codec},
    net,
    present::{Presenter, is_minimized},
    proto::{CAN_HEVC, CAN_HEVC444, HostStats, Msg, read_msg},
    stats::{now_us, percentile},
    toolbar,
    video_rx::{self, Delivered, FrameQueue, RxCounters},
    window::{self, WindowState},
};

#[derive(Args)]
pub struct ViewArgs {
    /// Host device id.
    id: EndpointId,
    /// Direct address of the host (repeatable), e.g. a LAN or EasyTier IP:port.
    #[arg(long)]
    addr: Vec<SocketAddr>,
    /// A relay the host can be reached through (repeatable).
    #[arg(long)]
    via: Vec<RelayUrl>,
    /// This computer's own relay (repeatable).
    #[arg(long, value_delimiter = ',')]
    relay: Vec<RelayUrl>,
    /// Use no public relay (to meet or to find a direct way).
    #[arg(long)]
    no_public: bool,
    /// Only relays, no direct addresses (for tests).
    #[arg(long)]
    relay_only: bool,
    /// The host's name, for the window title.
    #[arg(long)]
    name: Option<String>,
    /// Highest bitrate in Mbit/s, instead of this device's choice in the Display
    /// menu (for tests; not saved).
    #[arg(long)]
    mbps: Option<f64>,
    /// Frames per second (0 = this screen's refresh rate), instead of this
    /// device's choice (for tests; not saved).
    #[arg(long)]
    fps: Option<u8>,
    /// Device key file (default: %APPDATA%\remote-control\device.key); the
    /// session signs its proof with it, unless --proof-stdin.
    #[arg(long)]
    key: Option<PathBuf>,
    /// Read the session key (hex) and its device proof from stdin, one per
    /// line (the lobby; with the service, only the service can read the device key).
    #[arg(long)]
    proof_stdin: bool,
    /// Close after this many seconds (for tests).
    #[arg(long)]
    seconds: Option<u64>,
    /// Move the host's pointer to a few points and check where it lands (for tests).
    #[arg(long)]
    probe: bool,
    /// Watch only: send no mouse or keyboard input (needed when host and
    /// viewer run on the same computer, or input would loop back).
    #[arg(long)]
    view_only: bool,
    /// Append one line of statistics per second to this CSV file.
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Lowest pacing rate, as a multiple of 2 Mbps; 0 = plain BBR (for comparison).
    #[arg(long, default_value_t = 4.0)]
    floor_pacing: f64,
    /// Run lines appended to this file during the session (for tests):
    /// `move X Y` and `click X Y` (fractions of the picture), `keys SCAN...`
    /// (hex scan codes, pressed in order, released in reverse), `sas`,
    /// `monitor next|prev|N` (N from 1, may be out of range), `probe` (as --probe, now),
    /// `shot FILE.bmp` (save the picture shown now, half size), `picture res|fps|mbps V`
    /// (as the Display menu, see `picture_line`), `full` (full screen on or off).
    #[arg(long)]
    script: Option<PathBuf>,
    /// The keyboard hook also takes injected keys marked as test keys, and
    /// logs every key it routes (for tests).
    #[arg(long, hide = true)]
    hook_test: bool,
    /// Save a picture of each cursor built from the host's shapes in this folder (for tests).
    #[arg(long, hide = true)]
    cursor_dump: Option<PathBuf>,
    /// Drop this percentage of the received video datagrams (for tests).
    #[arg(long, default_value_t = 0.0)]
    sim_loss: f64,
    /// With --sim-loss: mean number of datagrams lost in a row (1 = each on its own).
    #[arg(long, default_value_t = 1.0)]
    sim_burst: f64,
}

/// Filled by the render thread, emptied once a second.
#[derive(Default)]
struct RenderSecond {
    presented: u64,
    decoded: u64,
    decode_ms: Vec<f64>,
    /// Frame complete (all shards here) to Present returned.
    local_ms: Vec<f64>,
    host_ms: Vec<f64>,
    errors: u64,
    /// Longest time between two shown pictures that ended in this second.
    max_gap_ms: f64,
}

struct Shared {
    ctrl: mpsc::UnboundedSender<Msg>,
    active_conn: Mutex<Connection>,
    reconnecting: AtomicBool,
    quit: AtomicBool,
    render: Mutex<RenderSecond>,
    host: Mutex<Option<HostStats>>,
    input_rtt: Mutex<Vec<f64>>,
    probe: Mutex<Vec<(u16, u16)>>,
    first_present: Mutex<Option<Instant>>,
    last_picture: Mutex<Option<Instant>>,
    /// The last control message from the host (it answers a ping every 200 ms).
    last_heard: Mutex<Instant>,
    /// Closed here because the host went silent, see `HOST_SILENT`.
    silent: AtomicBool,
    /// `shot` from `--script`: where to save the next shown picture.
    shot: Mutex<Option<PathBuf>>,
    /// The host asks its user first (`Msg::Waiting`).
    waiting: AtomicBool,
    /// The host paused the picture: only a public relay between us.
    paused: AtomicBool,
    /// Why the host sends no picture now (`proto::HINT_*`).
    hint: std::sync::atomic::AtomicU8,
    /// When the host said Ctrl+Alt+Del did not go through.
    no_sas: Mutex<Option<Instant>>,
    /// Currently streamed host monitor index (-1 if unknown).
    current_monitor: AtomicI32,
    /// Monitor index to restore after a handoff reconnect (-1 if none pending).
    restore_monitor: AtomicI32,
}

/// How long "Ctrl+Alt+Del did not go through" stays up.
const NO_SAS_SHOWN: Duration = Duration::from_secs(6);

/// The badge and the lines under it for a hint from the host.
fn hint_text(what: u8) -> Option<(&'static str, Vec<(String, String)>)> {
    use crate::proto::{HINT_LOCKED, HINT_NO_DISPLAY, HINT_NO_SAS, HINT_UAC};
    let row = |k: &str, v: &str| (k.to_string(), v.to_string());
    Some(match what {
        HINT_NO_DISPLAY => (
            tr("对方没有可用的屏幕", "No display available on remote PC"),
            vec![
                row(
                    tr("原因", "Reason"),
                    tr("笔记本合上了盖子，又没接显示器", "Laptop lid is closed with no external monitor"),
                ),
                row(
                    tr("画面", "Display"),
                    tr("打开盖子或接上显示器后自动回来", "Will return automatically when lid opens or monitor connects"),
                ),
            ],
        ),
        HINT_UAC => (
            tr(
                "对方屏幕上有管理员确认，请对方在自己电脑上点“是”",
                "UAC prompt on remote PC — ask them to click Yes",
            ),
            vec![
                row(
                    tr("画面", "Display"),
                    tr("对方点完后自动回来", "Will return automatically once confirmed"),
                ),
                row(
                    tr("以后", "Tip"),
                    tr(
                        "对方装上后台服务，就能远程点管理员确认",
                        "Install the background service on remote PC to interact with UAC",
                    ),
                ),
            ],
        ),
        HINT_LOCKED => (
            tr("对方已锁屏，请对方解锁", "Remote PC is locked — ask them to unlock"),
            vec![
                row(
                    tr("画面", "Display"),
                    tr("对方解锁后自动回来", "Will return automatically once unlocked"),
                ),
                row(
                    tr("以后", "Tip"),
                    tr(
                        "对方装上后台服务，锁屏时也能远程解锁",
                        "Install the background service on remote PC to unlock remotely",
                    ),
                ),
            ],
        ),
        HINT_NO_SAS => (
            tr("Ctrl+Alt+Del 没发过去", "Could not send Ctrl+Alt+Del"),
            vec![row(
                tr("原因", "Reason"),
                tr(
                    "对方没装后台服务；装上后才能远程按 Ctrl+Alt+Del",
                    "Background service is not installed on remote PC",
                ),
            )],
        ),
        _ => return None,
    })
}

/// One line for the lobby, which reads this window's output: `@status
/// connecting|waiting|connected|paused|resumed`, or `@status failed|closed\t<why>`.
fn status(s: &str) {
    println!("@status {s}");
}

/// The session's own key and its device proof (see `ident`).
fn session_key(a: &ViewArgs) -> Result<(SecretKey, String)> {
    if a.proof_stdin {
        let mut lines = std::io::stdin().lines();
        let mut next = || lines.next().transpose().ok().flatten().unwrap_or_default();
        let (hex, proof) = (next(), next());
        let bytes: Vec<u8> = (0..hex.trim().len() / 2)
            .filter_map(|i| u8::from_str_radix(hex.trim().get(i * 2..i * 2 + 2)?, 16).ok())
            .collect();
        let arr: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("--proof-stdin: bad session key"))?;
        return Ok((SecretKey::from_bytes(&arr), proof.trim().to_string()));
    }
    let device = net::load_key(&a.key.clone().unwrap_or_else(net::default_key_path))?;
    let session = SecretKey::generate();
    let proof = crate::ident::issue(&device, session.public());
    Ok((session, proof))
}

pub fn run(a: ViewArgs) -> Result<()> {
    crate::i18n::init_from_key(&a.key.clone().unwrap_or_else(net::default_key_path));
    let r = run_inner(&a);
    if let Err(e) = &r {
        status(&format!("failed\t{e:#}"));
    }
    r
}

fn run_inner(a: &ViewArgs) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let t_start = Instant::now();
    crate::term::set_session(a.id, &a.name.clone().unwrap_or_else(|| a.id.fmt_short().to_string()));
    status("connecting");
    // Whether HEVC (and its 4:4:4) decodes here, found out while connecting.
    let hevc = std::thread::spawn(|| -> Result<u8> {
        mf::init_thread()?;
        let gpu = Gpu::default_adapter()?;
        Decoder::available(&gpu.device, Codec::Hevc)?;
        Ok(CAN_HEVC | if Decoder::can_444(&gpu.device) { CAN_HEVC444 } else { 0 })
    });
    let (secret, proof) = session_key(a)?;
    crate::cc::set_pacing_gain(a.floor_pacing);
    let relays = net::Relays { own: a.relay.clone(), public: !a.no_public, theirs: a.via.clone(), token: Some(proof.clone()) };
    let ep = rt.block_on(net::endpoint(secret, &relays, !a.relay_only))?;
    let ips: &[SocketAddr] = if a.relay_only { &[] } else { &a.addr };
    let mut via = a.via.clone();
    if via.is_empty() {
        via = a.relay.clone();
        if !a.no_public && a.relay.is_empty() {
            via.extend(net::public_relays());
        }
    }
    let target = net::dial_addr(a.id, ips, &via);
    let dial = async { tokio::time::timeout(Duration::from_secs(15), ep.connect(target.clone(), crate::proto::ALPN)).await };
    let no_answer = tr(
        "连不上对方：没有回应（对方没开机、没联网、睡着了，或者找不到通路）",
        "Cannot connect: no response (remote PC is off, offline, asleep, or unreachable)",
    );
    let connected = match rt.block_on(dial) {
        Ok(Ok(conn)) => Ok(conn),
        // The handshake gave up: quinn's idle timeout.
        Ok(Err(e)) if format!("{e:#}").contains("timed out") => Err(anyhow!(no_answer)),
        Ok(Err(e)) => Err(anyhow!("{}{e:#}", tr("连不上对方：", "Cannot connect: "))),
        Err(_) => Err(anyhow!(no_answer)),
    };
    let r = connected.and_then(|conn| {
        let caps = match hevc.join() {
            Ok(Ok(caps)) => caps,
            Ok(Err(e)) => {
                log!("no HEVC decoding here ({e:#}); asking for H.264");
                0
            }
            Err(_) => 0,
        };
        run_connected(&rt, a, &ep, &target, &conn, t_start, caps, proof)
    });
    rt.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(2), ep.close()).await;
    });
    r
}

fn is_handoff_close(conn: &Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(ConnectionError::ApplicationClosed(c)) if c.error_code.into_inner() == u64::from(net::CLOSE_HANDOFF)
    )
}

fn terminal_close_code(reason: &ConnectionError) -> Option<u32> {
    if let ConnectionError::ApplicationClosed(c) = reason {
        let code = c.error_code.into_inner() as u32;
        if matches!(
            code,
            net::CLOSE_DENIED
                | net::CLOSE_NOT_MET
                | net::CLOSE_BUSY
                | net::CLOSE_LOCKED
                | net::CLOSE_DISCONNECTED
                | net::CLOSE_STOPPED
        ) {
            return Some(code);
        }
    }
    None
}

async fn check_terminal_refusal(conn: &Connection) -> Option<u32> {
    if conn.close_reason().is_none() {
        let _ = tokio::time::timeout(Duration::from_millis(300), conn.closed()).await;
    }
    conn.close_reason().as_ref().and_then(terminal_close_code)
}

fn make_sim(a: &ViewArgs) -> Option<crate::lossim::LossSim> {
    (a.sim_loss > 0.0).then(|| crate::lossim::LossSim::new(a.sim_loss, a.sim_burst, now_us() ^ 0x9E37_79B9_7F4A_7C15))
}

fn spawn_ctrl_read(
    rt: &tokio::runtime::Handle,
    sh: Arc<Shared>,
    mut recv: RecvStream,
    clip: Option<Arc<ClipSync>>,
    info_tx: mpsc::UnboundedSender<(u32, u32, String)>,
) -> AbortHandle {
    rt.spawn(async move {
        while let Ok(m) = read_msg(&mut recv).await {
            *sh.last_heard.lock().unwrap() = Instant::now();
            match m {
                Msg::Info { width, height, encoder, streaming } => {
                    if let Some(s) = streaming {
                        log!(
                            "host streams {width}x{height} at up to {} fps and {:.1} Mbps, {}",
                            s.fps,
                            s.max_kbps as f64 / 1000.0,
                            if s.yuv444 { "4:4:4" } else if s.can444 { "4:2:0 (4:4:4 on offer)" } else { "4:2:0" }
                        );
                    }
                    crate::quality::set_streaming((width as u32, height as u32), streaming);
                    let _ = info_tx.send((width as u32, height as u32, encoder));
                }
                Msg::Stats(s) => *sh.host.lock().unwrap() = Some(s),
                Msg::Waiting => {
                    sh.waiting.store(true, Ordering::SeqCst);
                    log!("the host asks its user");
                    status("waiting");
                }
                Msg::Paused { on } => {
                    sh.paused.store(on, Ordering::SeqCst);
                    log!("picture {}", if on { "paused by the host: only a public relay between us" } else { "goes on" });
                    status(if on { "paused" } else { "resumed" });
                }
                Msg::Hint { what } if what == crate::proto::HINT_NO_SAS => {
                    log!("the host could not press Ctrl+Alt+Del (no service there)");
                    *sh.no_sas.lock().unwrap() = Some(Instant::now());
                }
                Msg::Hint { what } => {
                    log!("host hint: {}", hint_text(what).map_or("picture back", |t| t.0));
                    sh.hint.store(what, Ordering::SeqCst);
                }
                Msg::Pong { t_us } => {
                    sh.input_rtt.lock().unwrap().push(now_us().saturating_sub(t_us) as f64 / 1000.0)
                }
                Msg::ProbeReply { x, y } => sh.probe.lock().unwrap().push((x, y)),
                Msg::Desktop { name } => log!("host desktop: {name}"),
                Msg::Monitors { list, current } => {
                    let names: Vec<String> = list
                        .iter()
                        .map(|m| format!("{} {}x{} at ({}, {})", m.name, m.width, m.height, m.x, m.y))
                        .collect();
                    log!("host displays: {} (streaming {})", names.join("; "), current as u32 + 1);
                    sh.current_monitor.store(current as i32, Ordering::SeqCst);
                    window::set_monitors(current as usize, list.len());
                    let want = sh.restore_monitor.swap(-1, Ordering::SeqCst);
                    if want >= 0 && want as usize != current as usize && (want as usize) < list.len() {
                        let _ = sh.ctrl.send(Msg::SelectMonitor { index: want as u8 });
                    }
                }
                m @ (Msg::CursorShape { .. } | Msg::CursorUse { .. } | Msg::CursorHidden) => window::remote_cursor(m),
                Msg::ClipOffer { seq, .. } => {
                    if let Some(c) = &clip {
                        c.on_offer(seq);
                    }
                }
                _ => {}
            }
        }
    })
    .abort_handle()
}

fn spawn_watchdog(rt: &tokio::runtime::Handle, sh: Arc<Shared>, conn: Connection) -> AbortHandle {
    rt.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        while conn.close_reason().is_none() {
            tick.tick().await;
            let quiet = sh.last_heard.lock().unwrap().elapsed();
            if quiet >= HOST_SILENT {
                log!("nothing from the host for {:.1} s: closing", quiet.as_secs_f64());
                sh.silent.store(true, Ordering::SeqCst);
                conn.close(0u32.into(), b"host silent");
            }
        }
    })
    .abort_handle()
}

#[allow(clippy::too_many_arguments)]
fn run_connected(
    rt: &tokio::runtime::Runtime,
    a: &ViewArgs,
    ep: &Endpoint,
    target: &EndpointAddr,
    initial_conn: &Connection,
    t_start: Instant,
    caps: u8,
    proof: String,
) -> Result<()> {
    let _timer = crate::audio::TimerPeriodGuard::enter(1);
    let mut conn = initial_conn.clone();
    if let Some(dir) = &a.cursor_dump {
        std::fs::create_dir_all(dir)?;
        window::set_cursor_dump(dir.clone());
    }
    let (k, addr, rtt) = net::selected_path(&conn);
    log!("connected in {} ms via {} {addr} (rtt {rtt:.0} ms)", t_start.elapsed().as_millis(), k.label());

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Msg>();
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Option<SendStream>>();
    rt.spawn(async move {
        // Coalesce high-polling-rate mouse moves (e.g. 500–1000 Hz gaming mice)
        // to ~125 Hz during continuous motion while sending the first move
        // after idle immediately (0 ms) and flushing any pending move right
        // before clicks, keys, or wheels. Prevents both half-duplex Wi-Fi
        // uplink floods and Windows 11 Explorer/WebView2 drag stutter.
        const MOUSE_PACE: Duration = Duration::from_millis(8);
        let mut current: Option<SendStream> = None;
        let mut batch: Vec<Msg> = Vec::with_capacity(16);
        let mut wire: Vec<u8> = Vec::with_capacity(256);
        let mut pending_move: Option<(u16, u16)> = None;
        let mut last_move_sent: Option<Instant> = None;
        loop {
            tokio::select! {
                biased;
                w = writer_rx.recv() => match w {
                    Some(s) => {
                        if let Some(ref stream) = s {
                            // Prioritize input/control ahead of audio and clipboard streams.
                            let _ = stream.set_priority(100);
                        }
                        current = s;
                    }
                    None => break,
                },
                m = ctrl_rx.recv() => {
                    let Some(first) = m else { break };
                    batch.clear();
                    batch.push(first);
                    while let Ok(next) = ctrl_rx.try_recv() {
                        batch.push(next);
                    }
                    coalesce_ctrl(&mut batch);
                    if batch.len() == 1
                        && let Msg::MouseMove { x, y } = batch[0]
                    {
                        if last_move_sent.is_some_and(|t| t.elapsed() < MOUSE_PACE) {
                            pending_move = Some((x, y));
                            continue;
                        }
                        pending_move = None;
                        last_move_sent = Some(Instant::now());
                    } else {
                        if let Some((px, py)) = pending_move.take()
                            && !matches!(batch.first(), Some(Msg::MouseMove { .. }))
                        {
                            batch.insert(0, Msg::MouseMove { x: px, y: py });
                        }
                        if batch.iter().any(|m| matches!(m, Msg::MouseMove { .. })) {
                            last_move_sent = Some(Instant::now());
                        }
                    }
                    if let Some(s) = current.as_mut() {
                        wire.clear();
                        for msg in batch.drain(..) {
                            wire.extend_from_slice(&msg.encode());
                        }
                        if s.write_all(&wire).await.is_err() {
                            current = None;
                        }
                    } else {
                        batch.clear();
                    }
                }
                _ = async {
                    let due = last_move_sent.map_or_else(Instant::now, |t| t + MOUSE_PACE);
                    tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                }, if pending_move.is_some() => {
                    if let Some((x, y)) = pending_move.take() {
                        last_move_sent = Some(Instant::now());
                        if let Some(s) = current.as_mut()
                            && s.write_all(&Msg::MouseMove { x, y }.encode()).await.is_err()
                        {
                            current = None;
                        }
                    }
                }
            }
        }
    });

    let sh = Arc::new(Shared {
        ctrl: ctrl_tx.clone(),
        active_conn: Mutex::new(conn.clone()),
        reconnecting: AtomicBool::new(false),
        quit: AtomicBool::new(false),
        render: Mutex::default(),
        host: Mutex::default(),
        input_rtt: Mutex::default(),
        probe: Mutex::default(),
        first_present: Mutex::default(),
        last_picture: Mutex::default(),
        last_heard: Mutex::new(Instant::now()),
        silent: AtomicBool::new(false),
        shot: Mutex::default(),
        waiting: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        hint: Default::default(),
        no_sas: Mutex::default(),
        current_monitor: AtomicI32::new(-1),
        restore_monitor: AtomicI32::new(-1),
    });
    let (_, init_quality) = crate::quality::start(&a.id.to_string(), a.fps, a.mbps);
    let init_screen = crate::quality::screen();
    crate::quality::connect(ctrl_tx.clone());
    let _init_audio = crate::audio::config::start(&a.id.to_string());
    crate::audio::config::connect(ctrl_tx.clone());
    let (audio_tx, audio_rx) = mpsc::unbounded_channel::<bytes::Bytes>();
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio_reset = Arc::new(AtomicBool::new(false));
    let audio_player = {
        let (stop, reset) = (audio_stop.clone(), audio_reset.clone());
        std::thread::Builder::new().name("audio-play".into()).spawn(move || {
            crate::audio::wasapi::Player::run_with_reset(audio_rx, stop, reset);
        })?
    };

    let frames = Arc::new(FrameQueue::default());
    let counters = Arc::new(Mutex::new(RxCounters::default()));
    let sim = make_sim(a);
    if let Some(s) = &sim {
        log!("simulated loss: {} of the video datagrams", s.describe());
    }

    // Without a confirmation the picture information comes within a second;
    // with one, after the host's user answered (30 s at most). A host that
    // says why no picture can come (locked, no display) gets the window at
    // once, at a size of its own until the picture comes.
    let mut handoff_deadline: Option<Instant> = None;
    let (sw, sh_, encoder, mut read_handle, mut video_handle, mut uni_handle) = loop {
        let mut open_err: Option<String> = None;
        let streams = match rt.block_on(conn.open_bi()) {
            Ok((mut send, recv)) => {
                let choice = crate::quality::choice();
                // Hosts before the 画面 menu read only the first two: 自动 was their 20 Mbps.
                let old_kbps = if choice.mbps == 0 { 20_000 } else { choice.mbps as u32 * 1000 };
                let hello = Msg::Hello {
                    max_kbps: old_kbps,
                    fps: init_quality.fps,
                    caps,
                    proof: proof.clone(),
                    quality: Some(choice.quality(init_screen)),
                };
                match rt.block_on(async {
                    send.write_all(&hello.encode()).await?;
                    send.write_all(&Msg::Audio(crate::audio::config::choice()).encode()).await
                }) {
                    Ok(()) => Some((send, recv)),
                    Err(e) => {
                        open_err = Some(format!("send hello: {e:#}"));
                        None
                    }
                }
            }
            Err(e) => {
                open_err = Some(format!("open control stream: {e:#}"));
                None
            }
        };

        let mut got: Option<(u32, u32, String, AbortHandle, AbortHandle, AbortHandle)> = None;
        if let Some((send, recv)) = streams {
            sh.hint.store(crate::proto::HINT_NONE, Ordering::SeqCst);
            sh.waiting.store(false, Ordering::SeqCst);
            sh.paused.store(false, Ordering::SeqCst);
            let (info_tx, mut info_rx) = mpsc::unbounded_channel::<(u32, u32, String)>();
            let clip = {
                let _rt = rt.enter();
                match ClipSync::start_clip(&conn, ctrl_tx.clone(), false, false) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        log!("clipboard sharing off: {e:#}");
                        None
                    }
                }
            };
            let u_handle = rt
                .spawn(crate::clipsync::accept_uni_loop(
                    conn.clone(),
                    clip.as_ref().map(Arc::downgrade),
                    Some(audio_tx.clone()),
                ))
                .abort_handle();
            let r_handle = spawn_ctrl_read(rt.handle(), sh.clone(), recv, clip, info_tx);
            let v_handle = rt
                .spawn(video_rx::run(conn.clone(), frames.clone(), ctrl_tx.clone(), counters.clone(), make_sim(a), Some(audio_tx.clone())))
                .abort_handle();
            let asked = Instant::now();
            let res = rt.block_on(async {
                loop {
                    if sh.hint.load(Ordering::SeqCst) != crate::proto::HINT_NONE {
                        break Ok((1280, 720, String::new()));
                    }
                    let limit = if sh.waiting.load(Ordering::SeqCst) { 40 } else { 10 };
                    if conn.close_reason().is_some() || asked.elapsed() > Duration::from_secs(limit) {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let msg = if is_en() {
                            format!("No video from remote PC within {limit}s")
                        } else {
                            format!("对方 {limit} 秒内没有发来画面")
                        };
                        break Err(msg);
                    }
                    tokio::select! {
                        r = info_rx.recv() => {
                            if let Some(i) = r {
                                break Ok(i);
                            }
                        }
                        _ = conn.closed() => {}
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
            });
            match res {
                Ok((w, h, enc)) => {
                    *sh.active_conn.lock().unwrap() = conn.clone();
                    *sh.last_heard.lock().unwrap() = Instant::now();
                    let _ = writer_tx.send(Some(send));
                    got = Some((w, h, enc, r_handle, v_handle, u_handle));
                }
                Err(fallback_why) => {
                    r_handle.abort();
                    v_handle.abort();
                    u_handle.abort();
                    if conn.close_reason().is_none() {
                        conn.close(0u32.into(), b"timeout");
                    }
                    open_err = Some(fallback_why);
                }
            }
        } else {
            rt.block_on(async {
                if conn.close_reason().is_none() {
                    let _ = tokio::time::timeout(Duration::from_millis(300), conn.closed()).await;
                }
            });
        }

        if let Some((w, h, enc, r_h, v_h, u_h)) = got {
            break (w, h, enc, r_h, v_h, u_h);
        }

        if is_handoff_close(&conn) {
            let dl = *handoff_deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(45));
            log!("host is handing off during initial connect; retrying...");
            conn = rt.block_on(async {
                loop {
                    let now = Instant::now();
                    if now >= dl {
                        return Err(anyhow!("{}", close_text(net::CLOSE_HANDOFF)));
                    }
                    tokio::time::sleep(Duration::from_secs(1).min(dl - now)).await;
                    match tokio::time::timeout(Duration::from_secs(5), ep.connect(target.clone(), crate::proto::ALPN)).await {
                        Ok(Ok(c)) => return Ok(c),
                        Ok(Err(e)) => log!("initial handoff reconnect dial failed: {e:#}"),
                        Err(_) => log!("initial handoff reconnect dial timed out"),
                    }
                }
            })?;
            continue;
        }
        bail!("{}", refusal(&conn).or(open_err).unwrap_or_else(|| tr("连接失败", "Connection failed").into()));
    };

    log!("stream {sw}x{sh_}, host encoder {encoder}");
    status("connected");
    let wstate = WindowState::new(ctrl_tx.clone(), (sw, sh_), !a.view_only, a.hook_test);
    let title = a.name.clone().unwrap_or_else(|| a.id.fmt_short().to_string());
    let hwnd = window::create(wstate.clone(), &title, window::initial_client_size((sw, sh_)))?;
    let hwnd_raw = hwnd.0 as isize;
    {
        let frames_wake = frames.clone();
        window::set_resize_wake(Arc::new(move || frames_wake.wake()));
    }

    let render = {
        let (sh, frames) = (sh.clone(), frames.clone());
        std::thread::Builder::new().name("render".into()).spawn(move || {
            if let Err(e) = render_loop(&sh, frames, HWND(hwnd_raw as *mut _)) {
                log!("render: {e:#}");
            }
        })?
    };
    {
        let sh = sh.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            while !sh.quit.load(Ordering::Relaxed) {
                tick.tick().await;
                let _ = sh.ctrl.send(Msg::Ping { t_us: now_us() });
            }
        });
    }
    {
        let (sh, wstate, counters) = (sh.clone(), wstate.clone(), counters.clone());
        let (csv, seconds, probe) = (a.csv.clone(), a.seconds, a.probe);
        rt.spawn(async move {
            if let Err(e) = stats_loop(sh, wstate, counters, encoder, csv, seconds, probe).await {
                log!("stats: {e:#}");
            }
        });
    }
    {
        let (sh, wstate, ep, target, proof, ctrl_tx, writer_tx, frames, counters, audio_tx, audio_reset) = (
            sh.clone(),
            wstate.clone(),
            ep.clone(),
            target.clone(),
            proof.clone(),
            ctrl_tx.clone(),
            writer_tx.clone(),
            frames.clone(),
            counters.clone(),
            audio_tx.clone(),
            audio_reset.clone(),
        );
        let sim_loss = a.sim_loss;
        let sim_burst = a.sim_burst;
        let rt_handle = rt.handle().clone();
        rt.spawn(async move {
            let mut watchdog_handle = spawn_watchdog(&rt_handle, sh.clone(), conn.clone());
            loop {
                let reason = conn.closed().await;
                let silent = sh.silent.load(Ordering::SeqCst);
                let handoff = !silent
                    && !sh.quit.load(Ordering::Relaxed)
                    && !wstate.closed.load(Ordering::Relaxed)
                    && matches!(
                        &reason,
                        ConnectionError::ApplicationClosed(c) if c.error_code.into_inner() == u64::from(net::CLOSE_HANDOFF)
                    );
                if !handoff {
                    let why = if silent { Some(silent_text()) } else { why_closed(&reason) };
                    let Some(why) = why else { return };
                    let since = sh.last_picture.lock().unwrap().map_or(f64::NAN, |t| t.elapsed().as_secs_f64());
                    log!("connection closed: {reason} ({why}); last picture {since:.2} s before");
                    status(&format!("closed\t{why}"));
                    crate::toolbar::closed(why);
                    tokio::time::sleep(CLOSED_NOTICE).await;
                    window::close();
                    return;
                }

                log!("host is handing off (CLOSE_HANDOFF); reconnecting...");
                let prev_monitor = sh.current_monitor.load(Ordering::SeqCst);
                sh.reconnecting.store(true, Ordering::SeqCst);
                status("connecting");
                crate::toolbar::update(
                    crate::toolbar::Tone::Fair,
                    tr("对方正在切换后台服务，正在重连…", "Remote PC switching to service, reconnecting…").into(),
                    vec![],
                    true,
                );
                let _ = writer_tx.send(None);
                read_handle.abort();
                video_handle.abort();
                uni_handle.abort();
                watchdog_handle.abort();

                let deadline = Instant::now() + Duration::from_secs(45);
                let mut terminal_code: Option<u32> = None;
                let mut reconnected: Option<(Connection, SendStream, AbortHandle, AbortHandle, AbortHandle)> = None;
                loop {
                    if sh.quit.load(Ordering::Relaxed) || wstate.closed.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1).min(deadline - now)).await;
                    if sh.quit.load(Ordering::Relaxed) || wstate.closed.load(Ordering::Relaxed) {
                        return;
                    }

                    let new_conn = match tokio::time::timeout(Duration::from_secs(5), ep.connect(target.clone(), crate::proto::ALPN)).await {
                        Ok(Ok(c)) => c,
                        Ok(Err(e)) => {
                            log!("reconnect dial failed: {e:#}");
                            continue;
                        }
                        Err(_) => {
                            log!("reconnect dial timed out");
                            continue;
                        }
                    };
                    let (mut new_send, new_recv) = match tokio::time::timeout(Duration::from_secs(5), new_conn.open_bi()).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            log!("reconnect open_bi failed: {e:#}");
                            if let Some(code) = check_terminal_refusal(&new_conn).await {
                                terminal_code = Some(code);
                                break;
                            }
                            new_conn.close(0u32.into(), b"retry");
                            continue;
                        }
                        Err(_) => {
                            log!("reconnect open_bi timed out");
                            new_conn.close(0u32.into(), b"retry");
                            continue;
                        }
                    };

                    let choice = crate::quality::choice();
                    let old_kbps = if choice.mbps == 0 { 20_000 } else { choice.mbps as u32 * 1000 };
                    let screen = match crate::quality::screen() {
                        s if s.size == (0, 0) => init_screen,
                        s => s,
                    };
                    let quality = choice.quality(screen);
                    let hello = Msg::Hello {
                        max_kbps: old_kbps,
                        fps: quality.fps,
                        caps,
                        proof: proof.clone(),
                        quality: Some(quality),
                    };
                    if let Err(e) = async {
                        new_send.write_all(&hello.encode()).await?;
                        new_send.write_all(&Msg::Audio(crate::audio::config::choice()).encode()).await
                    }
                    .await
                    {
                        log!("reconnect send hello failed: {e:#}");
                        if let Some(code) = check_terminal_refusal(&new_conn).await {
                            terminal_code = Some(code);
                            break;
                        }
                        new_conn.close(0u32.into(), b"retry");
                        continue;
                    }

                    sh.hint.store(crate::proto::HINT_NONE, Ordering::SeqCst);
                    sh.waiting.store(false, Ordering::SeqCst);
                    sh.paused.store(false, Ordering::SeqCst);
                    sh.restore_monitor.store(prev_monitor, Ordering::SeqCst);
                    let (info_tx, mut info_rx) = mpsc::unbounded_channel::<(u32, u32, String)>();
                    let new_clip = match ClipSync::start_clip(&new_conn, ctrl_tx.clone(), false, false) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            log!("clipboard sharing off: {e:#}");
                            None
                        }
                    };
                    let new_uni = tokio::spawn(crate::clipsync::accept_uni_loop(
                        new_conn.clone(),
                        new_clip.as_ref().map(Arc::downgrade),
                        Some(audio_tx.clone()),
                    ))
                    .abort_handle();
                    let new_read = spawn_ctrl_read(&rt_handle, sh.clone(), new_recv, new_clip, info_tx);
                    let sim = (sim_loss > 0.0)
                        .then(|| crate::lossim::LossSim::new(sim_loss, sim_burst, now_us() ^ 0x9E37_79B9_7F4A_7C15));
                    let new_video = tokio::spawn(video_rx::run(
                        new_conn.clone(),
                        frames.clone(),
                        ctrl_tx.clone(),
                        counters.clone(),
                        sim,
                        Some(audio_tx.clone()),
                    ))
                    .abort_handle();
                    let info_deadline = Instant::now() + Duration::from_secs(10);
                    let ready = loop {
                        if sh.quit.load(Ordering::Relaxed) || wstate.closed.load(Ordering::Relaxed) {
                            new_read.abort();
                            new_video.abort();
                            new_uni.abort();
                            new_conn.close(0u32.into(), b"bye");
                            return;
                        }
                        if sh.hint.load(Ordering::SeqCst) != crate::proto::HINT_NONE {
                            break true;
                        }
                        let rem = info_deadline.saturating_duration_since(Instant::now());
                        if rem.is_zero() {
                            break false;
                        }
                        tokio::select! {
                            r = info_rx.recv() => {
                                break r.is_some();
                            }
                            _ = new_conn.closed() => {
                                break false;
                            }
                            _ = tokio::time::sleep(rem.min(Duration::from_millis(100))) => {}
                        }
                    };
                    if ready {
                        reconnected = Some((new_conn, new_send, new_read, new_video, new_uni));
                        break;
                    }
                    new_read.abort();
                    new_video.abort();
                    new_uni.abort();
                    if let Some(code) = check_terminal_refusal(&new_conn).await {
                        terminal_code = Some(code);
                        break;
                    }
                    new_conn.close(0u32.into(), b"retry");
                }

                if let Some(code) = terminal_code {
                    let why = close_text(code);
                    log!("reconnect refused with close code {code} ({why})");
                    status(&format!("closed\t{why}"));
                    crate::toolbar::closed(why);
                    tokio::time::sleep(CLOSED_NOTICE).await;
                    window::close();
                    return;
                }
                let Some((new_conn, new_send, new_read, new_video, new_uni)) = reconnected else {
                    if !sh.quit.load(Ordering::Relaxed) && !wstate.closed.load(Ordering::Relaxed) {
                        log!("reconnect timed out after 45 s");
                        status(&format!("closed\t{}", tr("重连超时", "Reconnect timed out")));
                        crate::toolbar::closed(tr("重连超时，连接已断开", "Reconnect timed out; disconnected"));
                        tokio::time::sleep(CLOSED_NOTICE).await;
                        window::close();
                    }
                    return;
                };

                let (k, addr, rtt) = net::selected_path(&new_conn);
                log!("reconnected via {} {addr} (rtt {rtt:.0} ms)", k.label());
                audio_reset.store(true, Ordering::Release);
                *sh.active_conn.lock().unwrap() = new_conn.clone();
                let _ = writer_tx.send(Some(new_send));
                *sh.last_heard.lock().unwrap() = Instant::now();
                sh.silent.store(false, Ordering::SeqCst);
                sh.reconnecting.store(false, Ordering::SeqCst);
                status("connected");
                crate::toolbar::update(crate::toolbar::Tone::Good, tr("已重连", "Reconnected").into(), vec![], false);
                let new_watchdog = spawn_watchdog(&rt_handle, sh.clone(), new_conn.clone());

                conn = new_conn;
                read_handle = new_read;
                video_handle = new_video;
                uni_handle = new_uni;
                watchdog_handle = new_watchdog;
            }
        });
    }
    if let Some(path) = a.script.clone() {
        let sh = sh.clone();
        rt.spawn(async move { script_loop(&sh, &path).await });
    }
    window::run_loop();
    sh.quit.store(true, Ordering::SeqCst);
    frames.close();
    audio_stop.store(true, Ordering::SeqCst);
    let _ = ctrl_tx.send(Msg::ReleaseAll);
    let _ = render.join();
    let _ = audio_player.join();
    let final_conn = sh.active_conn.lock().unwrap().clone();
    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        final_conn.close(0u32.into(), b"bye");
    });
    Ok(())
}

/// How long the window stays open to show why the connection ended.
const CLOSED_NOTICE: Duration = Duration::from_secs(2);

/// The host answers the pings every 200 ms; this long without a word, it is
/// gone. Home WiFi stalls for up to about 1 s (phase 0, P1-1), so 6 s is
/// several stalls back to back. QUIC's idle timeout is longer
/// (`net::IDLE_TIMEOUT`) and comes later still for a killed host: noq restarts
/// the idle timer when a packet goes out on a path that heard something since
/// its last send, and iroh keeps trying paths.
const HOST_SILENT: Duration = Duration::from_secs(6);

fn silent_text() -> &'static str {
    tr("对方没有回应，连接已断开", "Remote PC stopped responding; disconnected")
}

/// Why the host closed the connection, in words.
pub fn close_text(code: u32) -> &'static str {
    match code {
        net::CLOSE_NOT_MET => tr("对方不认识本机，先贴对方的连接码", "Not paired yet — enter their connection code first"),
        net::CLOSE_BUSY => tr("对方正被别的设备控制", "Remote PC is being controlled by another device"),
        net::CLOSE_SHUTDOWN => tr("对方在重启或关机", "Remote PC is restarting or shutting down"),
        net::CLOSE_STOPPED => tr("对方关闭了软件", "Remote PC closed the app"),
        net::CLOSE_DENIED => tr("对方拒绝了", "Connection was denied"),
        net::CLOSE_NO_ANSWER => tr("对方没有确认", "No response on remote PC"),
        net::CLOSE_LOCKED => tr("对方锁屏了", "Remote PC is locked"),
        net::CLOSE_NO_DIRECT => tr("找不到直连", "Could not establish direct connection"),
        net::CLOSE_DISCONNECTED => tr("对方点了断开", "Disconnected by remote PC"),
        net::CLOSE_HANDOFF => tr("对方正在切换后台服务", "Remote PC is switching to background service"),
        _ => tr("对方结束了会话", "Session ended by remote PC"),
    }
}

/// Why the connection ended, for the window title; None when this side closed it.
fn why_closed(e: &iroh::endpoint::ConnectionError) -> Option<&'static str> {
    use iroh::endpoint::ConnectionError;
    Some(match e {
        ConnectionError::LocallyClosed => return None,
        ConnectionError::ApplicationClosed(c) => close_text(c.error_code.into_inner() as u32),
        ConnectionError::TimedOut => silent_text(),
        _ => tr("连接已断开", "Disconnected"),
    })
}

/// Why the host closed the connection, in words, if it did.
fn refusal(conn: &iroh::endpoint::Connection) -> Option<String> {
    use iroh::endpoint::ConnectionError;
    Some(match conn.close_reason()? {
        ConnectionError::ApplicationClosed(c) => close_text(c.error_code.into_inner() as u32).to_string(),
        ConnectionError::TimedOut => silent_text().to_string(),
        e => format!("{}{e}", tr("连接断开：", "Disconnected: ")),
    })
}

enum Show {
    Shown,
    Skipped,
    Closed,
}

struct DecodedFrame {
    pic: Picture,
    size: (u32, u32),
    complete_at: Instant,
    host_us: u32,
    frame: u32,
}

/// Presents; `Show::Closed` when the window is already gone (closing), which is not an error.
fn show(pres: &mut Presenter, gpu: &Gpu, p: &Picture, size: (u32, u32), hwnd: HWND) -> Result<Show> {
    match pres.present(gpu, p, size) {
        Ok(Some(())) => Ok(Show::Shown),
        Ok(None) => Ok(Show::Skipped),
        Err(_) if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool() => Ok(Show::Closed),
        Err(e) => Err(e),
    }
}

fn maybe_request_key(sh: &Shared, last_key_req: &mut Option<Instant>, now: Instant) {
    if last_key_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(300)) {
        *last_key_req = Some(now);
        let _ = sh.ctrl.send(Msg::RequestKeyframe);
    }
}

fn decode_batch(
    batch: Vec<Delivered>,
    sh: &Shared,
    gpu: &Gpu,
    pres: &mut Presenter,
    dec: &mut Option<(Decoder, (u32, u32), Codec, bool)>,
    last_size: Option<(u32, u32)>,
    last_key_req: &mut Option<Instant>,
) -> (Option<DecodedFrame>, bool) {
    let mut newest = None;
    let mut switched = false;
    for f in batch {
        let size = (f.header.width as u32, f.header.height as u32);
        let codec = if f.header.hevc { Codec::Hevc } else { Codec::H264 };
        if dec.as_ref().is_none_or(|d| d.1 != size || d.2 != codec || d.3 != f.header.yuv444) {
            if !f.header.key {
                maybe_request_key(sh, last_key_req, Instant::now());
                continue;
            }
            *dec = None;
            pres.clear_cache();
            match Decoder::new(&gpu.device, size.0, size.1, codec) {
                Ok(d) => {
                    *dec = Some((d, size, codec, f.header.yuv444));
                    switched = true;
                    if last_size.is_some_and(|l| l != size) {
                        log!("stream now {}x{}", size.0, size.1);
                    }
                    window::set_stream(size);
                }
                Err(e) => {
                    log!("decoder init ({}x{} {codec:?}): {e:#}", size.0, size.1);
                    sh.render.lock().unwrap().errors += 1;
                    maybe_request_key(sh, last_key_req, Instant::now());
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }
        }
        let t0 = Instant::now();
        let r = dec.as_mut().expect("created above").0.decode(&f.data);
        match r {
            Ok(Some(pic)) => {
                crate::trace::mark(f.frame, "decoded", None);
                let mut rs = sh.render.lock().unwrap();
                rs.decoded += 1;
                rs.decode_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                newest = Some(DecodedFrame { pic, size, complete_at: f.complete_at, host_us: f.header.host_us, frame: f.frame });
            }
            Ok(None) => {}
            Err(e) => {
                log!("decode: {e:#}");
                sh.render.lock().unwrap().errors += 1;
                *dec = None;
                pres.clear_cache();
                maybe_request_key(sh, last_key_req, Instant::now());
            }
        }
    }
    (newest, switched)
}

/// Decodes every frame (delta frames depend on each other) and presents the newest.
fn render_loop(sh: &Shared, frames: Arc<FrameQueue>, hwnd: HWND) -> Result<()> {
    mf::init_thread()?;
    let _wlan = crate::wlan::LowLatency::enter();
    let mut gpu = Gpu::default_adapter()?;
    let mut pres = Presenter::new(&gpu, hwnd)?;
    log!("decoding on {}", gpu.name);
    let mut dec: Option<(Decoder, (u32, u32), Codec, bool)> = None;
    let mut last: Option<(Picture, (u32, u32))> = None;
    let mut last_shown: Option<Instant> = None;
    let mut last_key_req: Option<Instant> = None;
    let mut dec_switched = false;
    let mut pending_present = false;
    let mut was_minimized = false;
    while !sh.quit.load(Ordering::Relaxed) {
        if let Some((p, s)) = &last
            && let Some(path) = sh.shot.lock().unwrap().take()
        {
            let r = crate::snapshot::save_bmp(&gpu, p, *s, &path);
            log!("shot {}: {r:?}", path.display());
        }
        let Some(batch) = frames.take_wait(Duration::from_millis(50)) else {
            break;
        };
        let minimized = is_minimized(hwnd);
        if was_minimized && !minimized && last.is_some() {
            pending_present = true;
        }
        was_minimized = minimized;

        let mut newest = None;
        if !batch.is_empty() {
            let (d, switched) = decode_batch(batch, sh, &gpu, &mut pres, &mut dec, last.as_ref().map(|l| l.1), &mut last_key_req);
            if switched {
                dec_switched = true;
            }
            newest = d;
        }

        if let Some(mut f) = newest {
            if minimized {
                last = Some((f.pic, f.size));
                pending_present = true;
                if dec_switched {
                    pres.clear_cache();
                    dec_switched = false;
                }
                continue;
            }
            if !pres.wait_ready(100) {
                last = Some((f.pic, f.size));
                pending_present = true;
                if dec_switched {
                    pres.clear_cache();
                    dec_switched = false;
                }
                continue;
            }
            // Swapchain is ready now; drain any newer frames that arrived during the wait.
            if let Some(more) = frames.take_wait(Duration::ZERO)
                && !more.is_empty()
            {
                let (newer, switched) =
                    decode_batch(more, sh, &gpu, &mut pres, &mut dec, last.as_ref().map(|l| l.1), &mut last_key_req);
                if switched {
                    dec_switched = true;
                }
                if let Some(n) = newer {
                    f = n;
                }
            }
            match show(&mut pres, &gpu, &f.pic, f.size, hwnd) {
                Ok(Show::Shown) => {
                    pending_present = false;
                    crate::trace::mark(f.frame, "shown", None);
                    let mut rs = sh.render.lock().unwrap();
                    rs.presented += 1;
                    rs.local_ms.push(f.complete_at.elapsed().as_secs_f64() * 1000.0);
                    rs.host_ms.push(f.host_us as f64 / 1000.0);
                    if let Some(t) = last_shown {
                        rs.max_gap_ms = rs.max_gap_ms.max(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    last_shown = Some(Instant::now());
                    *sh.last_picture.lock().unwrap() = last_shown;
                    drop(rs);
                    let mut first = sh.first_present.lock().unwrap();
                    if first.is_none() {
                        *first = Some(Instant::now());
                        log!("first picture shown");
                    }
                    drop(first);
                }
                Ok(Show::Skipped) => {
                    pending_present = true;
                }
                Ok(Show::Closed) => break,
                Err(e) => {
                    log!("present failed ({e:#}); rebuilding renderer");
                    sh.render.lock().unwrap().errors += 1;
                    dec = None;
                    last = None;
                    pending_present = false;
                    dec_switched = false;
                    pres.clear_cache();
                    if let Ok(g) = Gpu::default_adapter()
                        && let Ok(np) = Presenter::new(&g, hwnd)
                    {
                        gpu = g;
                        pres = np;
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    maybe_request_key(sh, &mut last_key_req, Instant::now());
                    continue;
                }
            }
            last = Some((f.pic, f.size));
            if dec_switched {
                // Drop SRVs cached for the previous decoder's `last` picture.
                pres.clear_cache();
                dec_switched = false;
            }
        } else if !minimized
            && let Some((p, s)) = &last
            && (pending_present || pres.resized())
        {
            match show(&mut pres, &gpu, p, *s, hwnd) {
                Ok(Show::Shown) => {
                    if pending_present {
                        sh.render.lock().unwrap().presented += 1;
                    }
                    pending_present = false;
                }
                Ok(Show::Skipped) => {
                    pending_present = true;
                }
                Ok(Show::Closed) => break,
                Err(e) => {
                    log!("present on resize failed ({e:#}); rebuilding renderer");
                    dec = None;
                    last = None;
                    pending_present = false;
                    dec_switched = false;
                    pres.clear_cache();
                    if let Ok(g) = Gpu::default_adapter()
                        && let Ok(np) = Presenter::new(&g, hwnd)
                    {
                        gpu = g;
                        pres = np;
                        maybe_request_key(sh, &mut last_key_req, Instant::now());
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    }
    Ok(())
}

const CSV_HEADER: &str = "t_s,fps,mbps,rate_mbps,path,path_rtt_ms,frame_rtt_p50_ms,frame_rtt_p95_ms,host_p50_ms,host_p95_ms,encode_p50_ms,decode_p50_ms,local_p50_ms,local_p95_ms,total_est_ms,input_p50_ms,input_p95_ms,skipped_pct,shrunk_pct,size_p50_pct,size_max_pct,key_requests,lost,decode_errors,recovered,failed,sim_dropped,fec_pct,max_gap_ms";

#[allow(clippy::too_many_arguments)]
async fn stats_loop(
    sh: Arc<Shared>,
    wstate: Arc<WindowState>,
    counters: Arc<Mutex<RxCounters>>,
    encoder: String,
    csv: Option<PathBuf>,
    seconds: Option<u64>,
    probe: bool,
) -> Result<()> {
    use std::io::Write;
    let mut file = match &csv {
        Some(p) => {
            let new = !p.exists();
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            if new {
                writeln!(f, "{CSV_HEADER}")?;
            }
            Some(f)
        }
        None => None,
    };
    let mut probe_done = !probe;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    for t in 1u64.. {
        tick.tick().await;
        if sh.quit.load(Ordering::Relaxed) || wstate.closed.load(Ordering::Relaxed) {
            return Ok(());
        }
        if sh.reconnecting.load(Ordering::Relaxed) {
            continue;
        }
        let conn = sh.active_conn.lock().unwrap().clone();
        // The task waiting on `closed()` shows why and closes the window.
        if conn.close_reason().is_some() {
            if is_handoff_close(&conn) {
                continue;
            }
            return Ok(());
        }
        let r = std::mem::take(&mut *sh.render.lock().unwrap());
        let host = sh.host.lock().unwrap().take().unwrap_or_default();
        let inp = std::mem::take(&mut *sh.input_rtt.lock().unwrap());
        let k = std::mem::take(&mut *counters.lock().unwrap());
        let (path, _, path_rtt) = net::selected_path(&conn);
        let ms = |us: u32| us as f64 / 1000.0;
        let (host_ms, local_ms) = (percentile(&r.host_ms, 50.0), percentile(&r.local_ms, 50.0));
        // One way for a whole frame: its round trip minus the ack's way back.
        let frame_rtt = ms(host.frame_rtt_p50_us);
        let net_ms = (frame_rtt - path_rtt / 2.0).max(path_rtt / 2.0);
        let total = host_ms + net_ms + local_ms;
        let (in50, in95) = (percentile(&inp, 50.0), percentile(&inp, 95.0));
        let skipped = host.skipped_pm as f64 / 10.0;
        let shrunk = host.shrunk_pm as f64 / 10.0;
        let mbps = k.bytes as f64 * 8.0 / 1e6;
        let rate_mbps = host.rate_kbps as f64 / 1000.0;
        let decode_ms = percentile(&r.decode_ms, 50.0);
        let (sw, sh_) = *wstate.stream.lock().unwrap();
        let path_ui = match path {
            net::PathKind::None => tr("无通路", "No path"),
            net::PathKind::Direct => tr("直连", "Direct"),
            net::PathKind::Relay => tr("中转", "Relay"),
            net::PathKind::Public => tr("公共节点", "Public relay"),
        };
        let rows = vec![
            (
                tr("画面", "Display").to_string(),
                if is_en() {
                    format!("{sw}×{sh_} · {} fps · {mbps:.1} Mbps", r.presented)
                } else {
                    format!("{sw}×{sh_} · {} 帧/秒 · {mbps:.1} Mbps", r.presented)
                },
            ),
            (
                tr("线路", "Route").to_string(),
                if is_en() {
                    format!("{path_ui} · RTT {path_rtt:.0} ms")
                } else {
                    format!("{path_ui} · 往返 {path_rtt:.0} ms")
                },
            ),
            (tr("画面延迟约", "Display latency").to_string(), format!("{total:.0} ms")),
            (
                tr("被控端 + 网络 + 本机", "Host + Net + Local").to_string(),
                format!("{host_ms:.1} + {net_ms:.1} + {local_ms:.1} ms"),
            ),
            (tr("键鼠往返", "Input RTT").to_string(), format!("{in50:.0} ms")),
            (tr("跳帧", "Skipped frames").to_string(), format!("{skipped:.1}%")),
        ];
        let paused = sh.paused.load(Ordering::Relaxed);
        let hint = hint_text(sh.hint.load(Ordering::Relaxed));
        let no_sas = sh.no_sas.lock().unwrap().is_some_and(|t| t.elapsed() < NO_SAS_SHOWN);
        let (tone, badge) = match path {
            _ if paused => (toolbar::Tone::Bad, tr("正在找直连…", "Finding direct path…").to_string()),
            net::PathKind::Direct => (toolbar::Tone::Good, format!("{} · {path_rtt:.0} ms", tr("直连", "Direct"))),
            net::PathKind::Relay => (toolbar::Tone::Fair, format!("{} · {path_rtt:.0} ms", tr("中转", "Relay"))),
            _ => (toolbar::Tone::Bad, path_ui.to_string()),
        };
        let shown = hint.or_else(|| no_sas.then(|| hint_text(crate::proto::HINT_NO_SAS)).flatten());
        match shown {
            Some((text, lines)) => toolbar::update(toolbar::Tone::Bad, text.to_string(), [lines, rows].concat(), true),
            None => toolbar::update(tone, badge, rows, paused),
        }
        let my_window = net::path_congestion(&conn).map_or(0, |c| c.0 / 1000);
        println!(
            "{t:>4}s {:>3} fps {mbps:>5.1}/{rate_mbps:>4.1} Mbps {} {path_rtt:>5.1} ms | host {host_ms:>5.1} (enc {:>4.1}) net {net_ms:>5.1} local {local_ms:>5.1} = {total:>5.1} ms | decode {decode_ms:>4.1} ms | input {in50:>5.1}/{in95:>5.1} ms | skip {skipped:>4.1}% shrunk {shrunk:>4.1}% | size {:>3}/{:>4}% | lost {} keyreq {} | fec {} rec {} fail {} gap {:.0} ms | own window {my_window} KB",
            r.presented,
            path.label(),
            ms(host.encode_p50_us),
            host.size_p50_pct,
            host.size_max_pct,
            k.lost,
            k.key_requests,
            k.fec_pct,
            k.recovered,
            k.failed,
            r.max_gap_ms,
        );
        if let Some(f) = file.as_mut() {
            writeln!(
                f,
                "{t},{},{mbps:.2},{rate_mbps:.2},{},{path_rtt:.2},{frame_rtt:.2},{:.2},{host_ms:.2},{:.2},{:.2},{decode_ms:.2},{local_ms:.2},{:.2},{total:.2},{in50:.2},{in95:.2},{skipped:.1},{shrunk:.1},{},{},{},{},{},{},{},{},{},{:.1}",
                r.presented,
                path.label(),
                ms(host.frame_rtt_p95_us),
                percentile(&r.host_ms, 95.0),
                ms(host.encode_p50_us),
                percentile(&r.local_ms, 95.0),
                host.size_p50_pct,
                host.size_max_pct,
                k.key_requests,
                k.lost,
                r.errors,
                k.recovered,
                k.failed,
                k.sim_dropped,
                k.fec_pct,
                r.max_gap_ms,
            )?;
        }
        if t == 1 {
            log!("encoder on host: {encoder}");
        }
        if !probe_done && sh.first_present.lock().unwrap().is_some() {
            probe_done = true;
            run_probe(&sh).await;
        }
        if seconds.is_some_and(|s| t >= s) {
            window::close();
            return Ok(());
        }
    }
    bail!("unreachable")
}

/// Moves the host's pointer to a few points and reports where it landed.
async fn run_probe(sh: &Shared) {
    let points = [(0.1, 0.1), (0.5, 0.5), (0.9, 0.2), (0.25, 0.85), (0.999, 0.999)];
    let mut worst: f64 = 0.0;
    for (fx, fy) in points {
        let (x, y) = ((fx * 65535.0) as u16, (fy * 65535.0) as u16);
        sh.probe.lock().unwrap().clear();
        let _ = sh.ctrl.send(Msg::Probe { x, y });
        let mut reply = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(p) = sh.probe.lock().unwrap().first().copied() {
                reply = Some(p);
                break;
            }
        }
        match reply {
            Some((rx, ry)) => {
                let ex = (rx as f64 - x as f64).abs() / 655.35;
                let ey = (ry as f64 - y as f64).abs() / 655.35;
                worst = worst.max(ex).max(ey);
                log!(
                    "probe ({fx:.3}, {fy:.3}) -> ({:.3}, {:.3}), error {ex:.2}% / {ey:.2}%",
                    rx as f64 / 65535.0,
                    ry as f64 / 65535.0
                );
            }
            None => log!("probe ({fx:.3}, {fy:.3}): no reply"),
        }
    }
    log!("probe worst error {worst:.2}% of the picture size");
}

/// Runs the lines of `--script` as they are appended (see `ViewArgs::script`).
async fn script_loop(sh: &Shared, path: &std::path::Path) {
    let mut done = 0usize;
    while !sh.quit.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let lines: Vec<&str> = text.split_inclusive('\n').filter(|l| l.ends_with('\n')).collect();
        for line in lines.iter().skip(done) {
            script_line(sh, line.trim()).await;
        }
        done = done.max(lines.len());
    }
}

/// `picture res auto|native|1440|1080|720`, `picture fps auto|N`, `picture mbps N`
/// (0 = 自动), `picture sharp on|off`: as a click in the 画面 menu.
fn picture_line(f: &[&str]) -> bool {
    use crate::quality::{self, Res};
    let res = match f.get(1).copied() {
        Some("auto") => Some(Res::Auto),
        Some("native") => Some(Res::Native),
        Some(v) => v.parse().ok().filter(|&v| v >= 240).map(Res::Short),
        None => None,
    };
    let num = f.get(1).and_then(|v| v.parse::<u16>().ok());
    match (f.first().copied(), res, num) {
        (Some("res"), Some(r), _) => quality::choose(|c| c.res = r),
        (Some("fps"), Some(Res::Auto), _) => quality::choose(|c| c.fps = 0),
        (Some("fps"), _, Some(n)) if n <= 255 => quality::choose(|c| c.fps = n as u8),
        (Some("mbps"), _, Some(n)) => quality::choose(|c| c.mbps = n),
        (Some("sharp"), _, _) if matches!(f.get(1).copied(), Some("on" | "off")) => {
            quality::choose(|c| c.sharp = f[1] == "on")
        }
        _ => return false,
    }
    true
}

async fn script_line(sh: &Shared, line: &str) {
    let f: Vec<&str> = line.split_whitespace().collect();
    let pos = |i: usize| f.get(i)?.parse::<f64>().ok().map(|v| (v.clamp(0.0, 1.0) * 65535.0).round() as u16);
    let pause = || tokio::time::sleep(Duration::from_millis(40));
    let ok = match f.first().copied() {
        Some(cmd @ ("move" | "click")) => match (pos(1), pos(2)) {
            (Some(x), Some(y)) => {
                let _ = sh.ctrl.send(Msg::MouseMove { x, y });
                if cmd == "click" {
                    pause().await;
                    let _ = sh.ctrl.send(Msg::MouseButton { button: 0, down: true });
                    pause().await;
                    let _ = sh.ctrl.send(Msg::MouseButton { button: 0, down: false });
                }
                true
            }
            _ => false,
        },
        Some("keys") => {
            let scans: Vec<u16> =
                f[1..].iter().filter_map(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok()).collect();
            for &scan in &scans {
                let _ = sh.ctrl.send(Msg::Key { scan, down: true });
                pause().await;
            }
            for &scan in scans.iter().rev() {
                let _ = sh.ctrl.send(Msg::Key { scan, down: false });
                pause().await;
            }
            !scans.is_empty()
        }
        Some("shot") if f.len() >= 2 => {
            // The rest of the line, so the path may contain spaces.
            *sh.shot.lock().unwrap() = Some(PathBuf::from(line["shot".len()..].trim()));
            true
        }
        Some("sas") => {
            let _ = sh.ctrl.send(Msg::ReleaseAll);
            let _ = sh.ctrl.send(Msg::SendSas);
            true
        }
        Some("monitor") => {
            use crate::keys::MonitorKey;
            let key = match f.get(1).copied() {
                Some("next") => Some(MonitorKey::Next),
                Some("prev") => Some(MonitorKey::Prev),
                Some(n) => n.parse::<u16>().ok().filter(|n| (1..=256).contains(n)).map(|n| MonitorKey::Pick((n - 1) as u8)),
                None => None,
            };
            if let Some(k) = key {
                window::pick_monitor(k);
            }
            key.is_some()
        }
        Some("probe") => {
            run_probe(sh).await;
            true
        }
        Some("picture") => picture_line(&f[1..]),
        Some("full") => {
            window::toggle_fullscreen();
            true
        }
        _ => false,
    };
    log!("script: {line}{}", if ok { "" } else { " (not understood)" });
}

/// Coalesces consecutive `Msg::MouseMove` entries in a queued control batch to
/// the latest pointer coordinate while preserving strict ordering around clicks,
/// keys, wheels, and every other control message.
fn coalesce_ctrl(batch: &mut Vec<Msg>) {
    if batch.len() <= 1 {
        return;
    }
    let mut write = 0usize;
    for read in 0..batch.len() {
        let is_move = matches!(batch[read], Msg::MouseMove { .. });
        if is_move && write > 0 && matches!(batch[write - 1], Msg::MouseMove { .. }) {
            batch.swap(write - 1, read);
        } else {
            if write != read {
                batch.swap(write, read);
            }
            write += 1;
        }
    }
    batch.truncate(write);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_ctrl_merges_adjacent_mouse_moves_only() {
        let mut batch = vec![
            Msg::MouseMove { x: 10, y: 10 },
            Msg::MouseMove { x: 20, y: 20 },
            Msg::MouseMove { x: 30, y: 30 },
            Msg::MouseButton { button: 0, down: true },
            Msg::MouseMove { x: 40, y: 40 },
            Msg::MouseMove { x: 50, y: 50 },
            Msg::MouseButton { button: 0, down: false },
            Msg::MouseMove { x: 60, y: 60 },
        ];
        coalesce_ctrl(&mut batch);
        assert_eq!(batch.len(), 5);
        assert!(matches!(batch[0], Msg::MouseMove { x: 30, y: 30 }));
        assert!(matches!(batch[1], Msg::MouseButton { button: 0, down: true }));
        assert!(matches!(batch[2], Msg::MouseMove { x: 50, y: 50 }));
        assert!(matches!(batch[3], Msg::MouseButton { button: 0, down: false }));
        assert!(matches!(batch[4], Msg::MouseMove { x: 60, y: 60 }));
    }
}

