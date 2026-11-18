//! `remote-control view <id>`: connects to a host and shows its screen in a window.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use clap::Args;
use iroh::{EndpointId, RelayUrl, SecretKey, endpoint::Connection};
use tokio::sync::mpsc;
use windows::Win32::Foundation::HWND;

use crate::{
    clipsync::ClipSync,
    decode::{Decoder, Picture},
    gpu::Gpu,
    log,
    mf::{self, Codec},
    net,
    present::Presenter,
    proto::{CAN_HEVC, HostStats, Msg, read_msg},
    stats::{now_us, percentile},
    toolbar,
    video_rx::{self, Delivered, RxCounters},
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
    #[arg(long, default_value_t = 20.0)]
    mbps: f64,
    #[arg(long, default_value_t = 60)]
    fps: u8,
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
    /// `shot FILE.bmp` (save the picture shown now, half size).
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
}

/// How long "Ctrl+Alt+Del did not go through" stays up.
const NO_SAS_SHOWN: Duration = Duration::from_secs(6);

/// The badge and the lines under it for a hint from the host.
fn hint_text(what: u8) -> Option<(&'static str, Vec<(String, String)>)> {
    use crate::proto::{HINT_LOCKED, HINT_NO_DISPLAY, HINT_NO_SAS, HINT_UAC};
    let row = |k: &str, v: &str| (k.to_string(), v.to_string());
    Some(match what {
        HINT_NO_DISPLAY => (
            "对方没有可用的屏幕",
            vec![row("原因", "笔记本合上了盖子，又没接显示器"), row("画面", "打开盖子或接上显示器后自动回来")],
        ),
        HINT_UAC => (
            "对方屏幕上有管理员确认，请对方在自己电脑上点“是”",
            vec![row("画面", "对方点完后自动回来"), row("以后", "对方装上后台服务，就能远程点管理员确认")],
        ),
        HINT_LOCKED => (
            "对方已锁屏，请对方解锁",
            vec![row("画面", "对方解锁后自动回来"), row("以后", "对方装上后台服务，锁屏时也能远程解锁")],
        ),
        HINT_NO_SAS => ("Ctrl+Alt+Del 没发过去", vec![row("原因", "对方没装后台服务；装上后才能远程按 Ctrl+Alt+Del")]),
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
    // Whether HEVC decodes here, found out while connecting.
    let hevc = std::thread::spawn(|| {
        mf::init_thread()?;
        let gpu = Gpu::default_adapter()?;
        Decoder::available(&gpu.device, Codec::Hevc)
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
    let dial = async { tokio::time::timeout(Duration::from_secs(15), ep.connect(target, crate::proto::ALPN)).await };
    let no_answer = "连不上对方：没有回应（对方没开机、没联网、睡着了，或者找不到通路）";
    let connected = match rt.block_on(dial) {
        Ok(Ok(conn)) => Ok(conn),
        // The handshake gave up: quinn's idle timeout.
        Ok(Err(e)) if format!("{e:#}").contains("timed out") => Err(anyhow!(no_answer)),
        Ok(Err(e)) => Err(anyhow!("连不上对方：{e:#}")),
        Err(_) => Err(anyhow!(no_answer)),
    };
    let r = connected.and_then(|conn| {
        let caps = match hevc.join() {
            Ok(Ok(())) => CAN_HEVC,
            Ok(Err(e)) => {
                log!("no HEVC decoding here ({e:#}); asking for H.264");
                0
            }
            Err(_) => 0,
        };
        let r = run_connected(&rt, a, &conn, t_start, caps, proof);
        rt.block_on(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            conn.close(0u32.into(), b"bye");
        });
        r
    });
    rt.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(2), ep.close()).await;
    });
    r
}

fn run_connected(
    rt: &tokio::runtime::Runtime,
    a: &ViewArgs,
    conn: &Connection,
    t_start: Instant,
    caps: u8,
    proof: String,
) -> Result<()> {
    let conn = conn.clone();
    if let Some(dir) = &a.cursor_dump {
        std::fs::create_dir_all(dir)?;
        window::set_cursor_dump(dir.clone());
    }
    let (k, addr, rtt) = net::selected_path(&conn);
    log!("connected in {} ms via {} {addr} (rtt {rtt:.0} ms)", t_start.elapsed().as_millis(), k.label());

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Msg>();
    let sh = Arc::new(Shared {
        ctrl: ctrl_tx.clone(),
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
    });
    let (info_tx, info_rx) = std::sync::mpsc::channel::<(u32, u32, String)>();
    let closed = |what: &str, e: &dyn std::fmt::Display| anyhow!("{}", refusal(&conn).unwrap_or_else(|| format!("{what}: {e:#}")));
    let (mut send, mut recv) = rt.block_on(conn.open_bi()).map_err(|e| closed("open control stream", &e))?;
    let hello = Msg::Hello { max_kbps: (a.mbps * 1000.0) as u32, fps: a.fps, caps, proof };
    rt.block_on(send.write_all(&hello.encode())).map_err(|e| closed("send hello", &e))?;
    rt.spawn(async move {
        while let Some(m) = ctrl_rx.recv().await {
            if send.write_all(&m.encode()).await.is_err() {
                break;
            }
        }
    });
    let clip = {
        let _rt = rt.enter();
        match ClipSync::start(&conn, ctrl_tx.clone(), false) {
            Ok(c) => Some(c),
            Err(e) => {
                log!("clipboard sharing off: {e:#}");
                None
            }
        }
    };
    {
        let sh = sh.clone();
        rt.spawn(async move {
            while let Ok(m) = read_msg(&mut recv).await {
                *sh.last_heard.lock().unwrap() = Instant::now();
                match m {
                    Msg::Info { width, height, encoder } => {
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
                        window::set_monitors(current as usize, list.len());
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
        });
    }
    let (frames_tx, frames_rx) = std::sync::mpsc::channel::<Delivered>();
    let counters = Arc::new(Mutex::new(RxCounters::default()));
    let sim = (a.sim_loss > 0.0).then(|| crate::lossim::LossSim::new(a.sim_loss, a.sim_burst, now_us() ^ 0x9E37_79B9_7F4A_7C15));
    if let Some(s) = &sim {
        log!("simulated loss: {} of the video datagrams", s.describe());
    }
    rt.spawn(video_rx::run(conn.clone(), frames_tx, ctrl_tx.clone(), counters.clone(), sim));

    // Without a confirmation the picture information comes within a second;
    // with one, after the host's user answered (30 s at most). A host that
    // says why no picture can come (locked, no display) gets the window at
    // once, at a size of its own until the picture comes.
    let asked = Instant::now();
    let (sw, sh_, encoder) = loop {
        match info_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(i) => break i,
            Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {}
        }
        if sh.hint.load(Ordering::SeqCst) != crate::proto::HINT_NONE {
            break (1280, 720, String::new());
        }
        let limit = if sh.waiting.load(Ordering::SeqCst) { 40 } else { 10 };
        if conn.close_reason().is_some() || asked.elapsed() > Duration::from_secs(limit) {
            // A refusal arrives as a close; give it a moment.
            std::thread::sleep(Duration::from_millis(300));
            bail!("{}", refusal(&conn).unwrap_or_else(|| format!("对方 {limit} 秒内没有发来画面")));
        }
    };
    log!("stream {sw}x{sh_}, host encoder {encoder}");
    status("connected");
    let wstate = WindowState::new(ctrl_tx.clone(), (sw, sh_), !a.view_only, a.hook_test);
    let title = a.name.clone().unwrap_or_else(|| a.id.fmt_short().to_string());
    let hwnd = window::create(wstate.clone(), &title, window::initial_client_size((sw, sh_)))?;
    let hwnd_raw = hwnd.0 as isize;

    let render = {
        let sh = sh.clone();
        std::thread::Builder::new().name("render".into()).spawn(move || {
            if let Err(e) = render_loop(&sh, frames_rx, HWND(hwnd_raw as *mut _)) {
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
        let (sh, conn, wstate) = (sh.clone(), conn.clone(), wstate.clone());
        let (csv, seconds, probe) = (a.csv.clone(), a.seconds, a.probe);
        rt.spawn(async move {
            if let Err(e) = stats_loop(sh, conn, wstate, counters, encoder, csv, seconds, probe).await {
                log!("stats: {e:#}");
            }
        });
    }
    {
        let (sh, conn) = (sh.clone(), conn.clone());
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
        });
    }
    {
        let (sh, conn) = (sh.clone(), conn.clone());
        rt.spawn(async move {
            let reason = conn.closed().await;
            let why = if sh.silent.load(Ordering::SeqCst) { Some(SILENT_TEXT) } else { why_closed(&reason) };
            let Some(why) = why else { return };
            let since = sh.last_picture.lock().unwrap().map_or(f64::NAN, |t| t.elapsed().as_secs_f64());
            log!("connection closed: {reason} ({why}); last picture {since:.2} s before");
            status(&format!("closed\t{why}"));
            crate::toolbar::closed(why);
            tokio::time::sleep(CLOSED_NOTICE).await;
            window::close();
        });
    }
    if let Some(path) = a.script.clone() {
        let sh = sh.clone();
        rt.spawn(async move { script_loop(&sh, &path).await });
    }
    window::run_loop();
    sh.quit.store(true, Ordering::SeqCst);
    let _ = ctrl_tx.send(Msg::ReleaseAll);
    let _ = render.join();
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
const SILENT_TEXT: &str = "对方没有回应，连接已断开";

/// Why the host closed the connection, in words.
pub fn close_text(code: u32) -> &'static str {
    match code {
        net::CLOSE_NOT_MET => "对方不认识本机，先贴对方的连接码",
        net::CLOSE_BUSY => "对方正被别的设备控制",
        net::CLOSE_SHUTDOWN => "对方在重启或关机",
        net::CLOSE_STOPPED => "对方关闭了软件",
        net::CLOSE_DENIED => "对方拒绝了",
        net::CLOSE_NO_ANSWER => "对方没有确认",
        net::CLOSE_LOCKED => "对方锁屏了",
        net::CLOSE_NO_DIRECT => "找不到直连",
        net::CLOSE_DISCONNECTED => "对方点了断开",
        _ => "对方结束了会话",
    }
}

/// Why the connection ended, for the window title; None when this side closed it.
fn why_closed(e: &iroh::endpoint::ConnectionError) -> Option<&'static str> {
    use iroh::endpoint::ConnectionError;
    Some(match e {
        ConnectionError::LocallyClosed => return None,
        ConnectionError::ApplicationClosed(c) => close_text(c.error_code.into_inner() as u32),
        ConnectionError::TimedOut => SILENT_TEXT,
        _ => "连接已断开",
    })
}

/// Why the host closed the connection, in words, if it did.
fn refusal(conn: &iroh::endpoint::Connection) -> Option<String> {
    use iroh::endpoint::ConnectionError;
    Some(match conn.close_reason()? {
        ConnectionError::ApplicationClosed(c) => close_text(c.error_code.into_inner() as u32).to_string(),
        ConnectionError::TimedOut => SILENT_TEXT.to_string(),
        e => format!("连接断开：{e}"),
    })
}

/// Presents; false when the window is already gone (closing), which is not an error.
fn show(pres: &mut Presenter, gpu: &Gpu, p: &Picture, size: (u32, u32), hwnd: HWND) -> Result<bool> {
    match pres.present(gpu, p, size) {
        Ok(()) => Ok(true),
        Err(_) if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool() => Ok(false),
        Err(e) => Err(e),
    }
}

/// Decodes every frame (delta frames depend on each other) and presents the newest.
fn render_loop(sh: &Shared, frames: Receiver<Delivered>, hwnd: HWND) -> Result<()> {
    mf::init_thread()?;
    let gpu = Gpu::default_adapter()?;
    let mut pres = Presenter::new(&gpu, hwnd)?;
    log!("decoding on {}", gpu.name);
    let mut dec: Option<(Decoder, (u32, u32), Codec)> = None;
    let mut last: Option<(Picture, (u32, u32))> = None;
    let mut last_shown: Option<Instant> = None;
    while !sh.quit.load(Ordering::Relaxed) {
        if let Some((p, s)) = &last
            && let Some(path) = sh.shot.lock().unwrap().take()
        {
            let r = crate::snapshot::save_bmp(&gpu, p, *s, &path);
            log!("shot {}: {r:?}", path.display());
        }
        let mut batch = Vec::new();
        match frames.recv_timeout(Duration::from_millis(30)) {
            Ok(f) => batch.push(f),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        batch.extend(frames.try_iter());
        if batch.is_empty() {
            // The window was resized while the picture is still: draw the last frame again.
            if let Some((p, s)) = &last
                && pres.resized()
            {
                show(&mut pres, &gpu, p, *s, hwnd)?;
            }
            continue;
        }
        let mut newest = None;
        for f in batch {
            let size = (f.header.width as u32, f.header.height as u32);
            let codec = if f.header.hevc { Codec::Hevc } else { Codec::H264 };
            if dec.as_ref().is_none_or(|d| d.1 != size || d.2 != codec) {
                if !f.header.key {
                    continue;
                }
                dec = Some((Decoder::new(&gpu.device, size.0, size.1, codec)?, size, codec));
                if last.as_ref().is_some_and(|l| l.1 != size) {
                    log!("stream now {}x{}", size.0, size.1);
                }
                window::set_stream(size);
            }
            let t0 = Instant::now();
            let r = dec.as_mut().expect("created above").0.decode(&f.data);
            match r {
                Ok(Some(p)) => {
                    crate::trace::mark(f.frame, "decoded", None);
                    let mut rs = sh.render.lock().unwrap();
                    rs.decoded += 1;
                    rs.decode_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                    newest = Some((p, size, f.complete_at, f.header.host_us, f.frame));
                }
                Ok(None) => {}
                Err(e) => {
                    log!("decode: {e:#}");
                    sh.render.lock().unwrap().errors += 1;
                    dec = None;
                    let _ = sh.ctrl.send(Msg::RequestKeyframe);
                }
            }
        }
        if let Some((p, size, complete_at, host_us, frame)) = newest {
            if !show(&mut pres, &gpu, &p, size, hwnd)? {
                break;
            }
            crate::trace::mark(frame, "shown", None);
            let mut rs = sh.render.lock().unwrap();
            rs.presented += 1;
            rs.local_ms.push(complete_at.elapsed().as_secs_f64() * 1000.0);
            rs.host_ms.push(host_us as f64 / 1000.0);
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
            last = Some((p, size));
        }
    }
    Ok(())
}

const CSV_HEADER: &str = "t_s,fps,mbps,rate_mbps,path,path_rtt_ms,frame_rtt_p50_ms,frame_rtt_p95_ms,host_p50_ms,host_p95_ms,encode_p50_ms,decode_p50_ms,local_p50_ms,local_p95_ms,total_est_ms,input_p50_ms,input_p95_ms,skipped_pct,shrunk_pct,size_p50_pct,size_max_pct,key_requests,lost,decode_errors,recovered,failed,sim_dropped,fec_pct,max_gap_ms";

#[allow(clippy::too_many_arguments)]
async fn stats_loop(
    sh: Arc<Shared>,
    conn: iroh::endpoint::Connection,
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
        // The task waiting on `closed()` shows why and closes the window.
        if conn.close_reason().is_some() {
            return Ok(());
        }
        if sh.quit.load(Ordering::Relaxed) || wstate.closed.load(Ordering::Relaxed) {
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
        let rows = vec![
            ("画面".to_string(), format!("{sw}×{sh_} · {} 帧/秒 · {mbps:.1} Mbps", r.presented)),
            ("线路".to_string(), format!("{} · 往返 {path_rtt:.0} ms", path.label())),
            ("画面延迟约".to_string(), format!("{total:.0} ms")),
            ("被控端 + 网络 + 本机".to_string(), format!("{host_ms:.1} + {net_ms:.1} + {local_ms:.1} ms")),
            ("键鼠往返".to_string(), format!("{in50:.0} ms")),
            ("跳帧".to_string(), format!("{skipped:.1}%")),
        ];
        let paused = sh.paused.load(Ordering::Relaxed);
        let hint = hint_text(sh.hint.load(Ordering::Relaxed));
        let no_sas = sh.no_sas.lock().unwrap().is_some_and(|t| t.elapsed() < NO_SAS_SHOWN);
        let (tone, badge) = match path {
            _ if paused => (toolbar::Tone::Bad, "正在找直连…".to_string()),
            net::PathKind::Direct => (toolbar::Tone::Good, format!("直连 · {path_rtt:.0} ms")),
            net::PathKind::Relay => (toolbar::Tone::Fair, format!("中转 · {path_rtt:.0} ms")),
            p => (toolbar::Tone::Bad, p.label().to_string()),
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
        _ => false,
    };
    log!("script: {line}{}", if ok { "" } else { " (not understood)" });
}
