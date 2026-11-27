//! Active desktop control session (`rc/desktop/3`): control stream + video FEC/reorder + input.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointId, RelayUrl, SecretKey,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
};
use rc_ident::proof::{DeviceProof, unix_now};
use rc_proto::{
    proto::{
        ALPN, AudioCfg, CAN_HEVC, CLOSE_BUSY, CLOSE_DENIED, CLOSE_DISCONNECTED, CLOSE_HANDOFF,
        CLOSE_LOCKED, CLOSE_NO_ANSWER, CLOSE_NO_DIRECT, CLOSE_NOT_MET, CLOSE_SHUTDOWN,
        CLOSE_STOPPED, CursorImage, FrameHeader, HINT_LOCKED, HINT_NO_DISPLAY, HINT_NONE,
        HINT_UAC, Msg, Quality, read_msg,
    },
    reorder::{
        CHECK_EVERY, Delivered, FRAME_LINGER, PROBE_DONE, REPORT_EVERY, RESEND_SLACK, Reorder,
    },
};
use rc_stream::{
    fec::Assembler,
    wire::{self, Ack, ProbeResult, Resend, ShardHeader, T_AUDIO, T_AUDIO_RED, T_PROBE},
};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::net_util;

/// Callbacks from Rust engine/session into Kotlin (or tests).
pub trait Callback: Send + Sync + 'static {
    fn on_devices_updated(&self, devices_json: &str);
    fn on_session_state(&self, state: &str, message: &str);
    fn on_stream_info(&self, width: i32, height: i32, encoder: &str, fps: i32, max_kbps: i32);
    fn on_monitors(&self, monitors_json: &str, current_index: i32);
    fn on_windows(&self, windows_json: &str);
    fn on_app_icon(&self, app_key: &str, png: &[u8]);
    fn on_window_thumb(&self, hwnd: u64, png: &[u8]);
    fn on_stats(&self, fps: i32, kbps: i32, rtt_ms: i32, path_kind: &str);
    fn on_hint(&self, code: i32, message: &str);
    fn on_notice(&self, message: &str);
    fn on_cursor_shape(&self, id: i32, width: i32, height: i32, hot_x: i32, hot_y: i32, rgba: &[u8]);
    fn on_cursor_use(&self, id: i32);
    fn on_cursor_hidden(&self);
    fn on_cursor_meta(&self, kind: i32);
    fn on_text_focus(&self, editable: bool);
    fn on_virtual_display(&self, active: bool, width: i32, height: i32, dpi_percent: i32);
    fn on_route_changed(&self, relay_only: bool);
    /// `codec`: 0 = H.264, 1 = HEVC
    fn on_video_frame(
        &self,
        codec: i32,
        key: bool,
        width: i32,
        height: i32,
        host_us: i64,
        data: &[u8],
    );
}

#[derive(Serialize)]
struct MonitorJson<'a> {
    index: usize,
    name: &'a str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    primary: bool,
    rotation: u16,
}

#[derive(Serialize)]
struct WindowJson<'a> {
    hwnd: u64,
    app_key: &'a str,
    app_name: &'a str,
    title: &'a str,
    monitor: u8,
    minimized: bool,
    rx: f32,
    ry: f32,
    rw: f32,
    rh: f32,
}

#[derive(Clone)]
struct SessionViewState {
    quality: Quality,
    monitor_index: Option<u8>,
    focus_window: Option<(u64, u16, u16)>,
}

pub struct ActiveSession {
    ctrl_tx: mpsc::UnboundedSender<Msg>,
    route_tx: mpsc::UnboundedSender<bool>,
    view_state: Arc<Mutex<SessionViewState>>,
    cancel: Arc<AtomicBool>,
    conn: Arc<Mutex<Option<Connection>>>,
    ep: Arc<Mutex<Option<Endpoint>>>,
    start_instant: Instant,
    task: tokio::task::JoinHandle<()>,
}

impl ActiveSession {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        device_secret: SecretKey,
        target: EndpointId,
        addrs: Vec<SocketAddr>,
        own_relays: Vec<RelayUrl>,
        extra_relays: Vec<RelayUrl>,
        fit_w: u16,
        fit_h: u16,
        fps: u8,
        relay_only: bool,
        cb: Arc<dyn Callback>,
    ) -> Self {
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<Msg>();
        let (route_tx, route_rx) = mpsc::unbounded_channel::<bool>();
        let target_fps = if fps == 0 { 60 } else { fps };
        let view_state = Arc::new(Mutex::new(SessionViewState {
            quality: Quality {
                fit_w,
                fit_h,
                short: 0,
                fps: target_fps,
                kbps: 0,
                yuv444: false,
                auto_fps: true,
            },
            monitor_index: None,
            focus_window: None,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let conn_slot: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
        let ep_slot: Arc<Mutex<Option<Endpoint>>> = Arc::new(Mutex::new(None));
        let start_instant = Instant::now();

        let task = tokio::spawn(run_session(
            device_secret,
            target,
            addrs,
            own_relays,
            extra_relays,
            relay_only,
            view_state.clone(),
            ctrl_tx.clone(),
            ctrl_rx,
            route_rx,
            cancel.clone(),
            conn_slot.clone(),
            ep_slot.clone(),
            cb,
        ));

        Self {
            ctrl_tx,
            route_tx,
            view_state,
            cancel,
            conn: conn_slot,
            ep: ep_slot,
            start_instant,
            task,
        }
    }

    pub fn send(&self, msg: Msg) {
        {
            let mut st = self.view_state.lock().unwrap();
            match &msg {
                Msg::Quality(q) => st.quality = q.clone(),
                Msg::SelectMonitor { index } => {
                    st.monitor_index = Some(*index);
                    st.focus_window = None;
                }
                Msg::FocusWindow { hwnd, vw, vh } => {
                    st.focus_window = if *hwnd == 0 {
                        None
                    } else {
                        Some((*hwnd, *vw, *vh))
                    };
                }
                _ => {}
            }
        }
        let _ = self.ctrl_tx.send(msg);
    }

    pub fn set_relay_only(&self, relay_only: bool) {
        let _ = self.route_tx.send(relay_only);
    }

    pub fn send_input(&self, msg: Msg) {
        let ms = self.start_instant.elapsed().as_millis() as u32;
        let _ = self.ctrl_tx.send(Msg::InputTick { ms });
        let _ = self.ctrl_tx.send(msg);
    }

    pub fn close(self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.ctrl_tx.send(Msg::ReleaseAll);
        if let Some(c) = self.conn.lock().unwrap().take() {
            c.close(0u32.into(), b"viewer disconnected");
        }
        if let Some(ep) = self.ep.lock().unwrap().take() {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), ep.close()).await;
            });
        }
        self.task.abort();
    }
}

fn close_reason_text(err: &ConnectionError) -> String {
    if let ConnectionError::ApplicationClosed(ac) = err {
        let code: u64 = ac.error_code.into_inner();
        return match code as u32 {
            0 => "已断开连接".into(),
            CLOSE_NOT_MET => "对方还不认识这台手机，请先保持同一局域网或粘贴连接码".into(),
            CLOSE_BUSY => "对方电脑正被其他设备控制，或正在等待确认".into(),
            CLOSE_SHUTDOWN => "对方电脑正在关机或重启".into(),
            CLOSE_STOPPED => "对方电脑的远控程序已关闭".into(),
            CLOSE_DENIED => "对方拒绝了连接请求".into(),
            CLOSE_NO_ANSWER => "对方 30 秒内未点击允许".into(),
            CLOSE_LOCKED => "对方电脑已锁屏（仅设为「我的电脑」可锁屏直连）".into(),
            CLOSE_NO_DIRECT => "未能建立直连或自建中转通道".into(),
            CLOSE_DISCONNECTED => "对方已主动断开连接".into(),
            CLOSE_HANDOFF => "对方服务正在切换，请重试".into(),
            other => format!("连接已关闭 (code {other})"),
        };
    }
    format!("连接已中断: {err}")
}

fn hint_text(what: u8) -> &'static str {
    match what {
        HINT_NONE => "",
        HINT_NO_DISPLAY => "对方电脑当前没有可用显示器",
        HINT_UAC => "对方处于 UAC 权限弹窗界面（需在电脑上操作）",
        HINT_LOCKED => "对方电脑已锁屏",
        _ => "对方暂时无法发送画面",
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_with_route(
    device_secret: &SecretKey,
    target: EndpointId,
    addrs: &[SocketAddr],
    own_relays: &[RelayUrl],
    extra_relays: &[RelayUrl],
    relay_only: bool,
    view_state: &Arc<Mutex<SessionViewState>>,
) -> Result<(Endpoint, Connection, SendStream, RecvStream), String> {
    let session_key = SecretKey::generate();
    let proof = DeviceProof::issue(device_secret, session_key.public(), unix_now() + 86_400).encode();
    let ep = net_util::create_session_endpoint(
        session_key,
        own_relays,
        extra_relays,
        relay_only,
        &proof,
    )
    .await
    .map_err(|e| format!("初始化线路失败: {e:#}"))?;

    let (dial_addrs, dial_relays): (Vec<SocketAddr>, Vec<RelayUrl>) = if relay_only {
        (
            Vec::new(),
            net_util::self_hosted_relays(own_relays, extra_relays),
        )
    } else {
        let mut r = Vec::new();
        for u in own_relays.iter().chain(extra_relays.iter()) {
            if !r.contains(u) {
                r.push(u.clone());
            }
        }
        (addrs.to_vec(), r)
    };

    let ea = net_util::endpoint_addr(target, &dial_addrs, &dial_relays);
    let dial_timeout = if relay_only {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(12)
    };

    let conn = match tokio::time::timeout(dial_timeout, ep.connect(ea, ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
            });
            return Err(format!("连接失败: {e:#}"));
        }
        Err(_) => {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
            });
            return Err(if relay_only {
                "自建中转连接超时，请确认电脑与手机已接入同一中转".into()
            } else {
                "连接超时，请确认电脑在线或已加入同一自建中转".into()
            });
        }
    };

    let (mut send, recv) = match tokio::time::timeout(Duration::from_secs(5), conn.open_bi()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            conn.close(0u32.into(), b"open_bi");
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
            });
            return Err(format!("打开控制通道失败: {e:#}"));
        }
        Err(_) => {
            conn.close(0u32.into(), b"open_bi_timeout");
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
            });
            return Err("打开控制通道超时".into());
        }
    };

    let vs = view_state.lock().unwrap().clone();
    let target_fps = if vs.quality.fps == 0 { 60 } else { vs.quality.fps };
    let hello = Msg::Hello {
        max_kbps: 20_000,
        fps: target_fps,
        caps: CAN_HEVC,
        proof,
        quality: Some(vs.quality),
    };
    if let Err(e) = send.write_all(&hello.encode()).await {
        conn.close(0u32.into(), b"hello");
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
        });
        return Err(format!("发送握手失败: {e:#}"));
    }
    let _ = send.write_all(&Msg::Audio(AudioCfg::default()).encode()).await;
    if let Some(idx) = vs.monitor_index
        && idx > 0
    {
        let _ = send.write_all(&Msg::SelectMonitor { index: idx }.encode()).await;
    }
    if let Some((hwnd, vw, vh)) = vs.focus_window {
        let _ = send
            .write_all(&Msg::FocusWindow { hwnd, vw, vh }.encode())
            .await;
    }

    Ok((ep, conn, send, recv))
}

struct ConnTasks {
    out_tx: mpsc::UnboundedSender<Msg>,
    inbound_rx: mpsc::UnboundedReceiver<Msg>,
    writer: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<()>,
    pinger: tokio::task::JoinHandle<()>,
    video: tokio::task::JoinHandle<()>,
}

impl ConnTasks {
    fn abort_all(self) {
        self.writer.abort();
        self.reader.abort();
        self.pinger.abort();
        self.video.abort();
    }
}

fn spawn_conn_tasks(
    conn: &Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    ctrl_tx: &mpsc::UnboundedSender<Msg>,
    host_resends: &Arc<AtomicBool>,
    frame_tx: &std::sync::mpsc::SyncSender<Delivered>,
) -> ConnTasks {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Msg>();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Msg>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if send.write_all(&msg.encode()).await.is_err() {
                break;
            }
        }
    });

    let reader = tokio::spawn(async move {
        while let Ok(msg) = read_msg(&mut recv).await {
            if inbound_tx.send(msg).is_err() {
                break;
            }
        }
    });

    let ping_tx = out_tx.clone();
    let ping_conn = conn.clone();
    let pinger = tokio::spawn(async move {
        let started = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if ping_conn.close_reason().is_some() {
                break;
            }
            let t_us = started.elapsed().as_micros() as u64;
            if ping_tx.send(Msg::Ping { t_us }).is_err() {
                break;
            }
        }
    });

    let video = tokio::spawn(run_video_rx(
        conn.clone(),
        ctrl_tx.clone(),
        host_resends.clone(),
        frame_tx.clone(),
    ));

    ConnTasks {
        out_tx,
        inbound_rx,
        writer,
        reader,
        pinger,
        video,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    device_secret: SecretKey,
    target: EndpointId,
    addrs: Vec<SocketAddr>,
    own_relays: Vec<RelayUrl>,
    extra_relays: Vec<RelayUrl>,
    initial_relay_only: bool,
    view_state: Arc<Mutex<SessionViewState>>,
    ctrl_tx: mpsc::UnboundedSender<Msg>,
    mut ctrl_rx: mpsc::UnboundedReceiver<Msg>,
    mut route_rx: mpsc::UnboundedReceiver<bool>,
    cancel: Arc<AtomicBool>,
    conn_slot: Arc<Mutex<Option<Connection>>>,
    ep_slot: Arc<Mutex<Option<Endpoint>>>,
    cb: Arc<dyn Callback>,
) {
    cb.on_session_state("connecting", "正在连接电脑…");

    let can_relay_only = net_util::has_self_hosted_relay(&own_relays, &extra_relays);
    let mut cur_relay_only = initial_relay_only && can_relay_only;
    if initial_relay_only && !can_relay_only {
        cb.on_route_changed(false);
    }

    let first = connect_with_route(
        &device_secret,
        target,
        &addrs,
        &own_relays,
        &extra_relays,
        cur_relay_only,
        &view_state,
    )
    .await;

    let (mut ep, mut conn, send, recv) = match first {
        Ok(parts) => parts,
        Err(_err) if cur_relay_only => {
            // Just like Windows viewer (`RELAY_TRY`): if saved relay-only fails at initial connect,
            // fall back automatically to Auto route so the user is never locked out.
            cur_relay_only = false;
            cb.on_route_changed(false);
            cb.on_notice("中转连接失败，已自动切回自动线路");
            match connect_with_route(
                &device_secret,
                target,
                &addrs,
                &own_relays,
                &extra_relays,
                false,
                &view_state,
            )
            .await
            {
                Ok(parts) => parts,
                Err(err2) => {
                    if !cancel.load(Ordering::Relaxed) {
                        cb.on_session_state("error", &err2);
                    }
                    return;
                }
            }
        }
        Err(err) => {
            if !cancel.load(Ordering::Relaxed) {
                cb.on_session_state("error", &err);
            }
            return;
        }
    };

    if cancel.load(Ordering::Relaxed) {
        conn.close(0u32.into(), b"cancelled");
        let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
        return;
    }
    *conn_slot.lock().unwrap() = Some(conn.clone());
    *ep_slot.lock().unwrap() = Some(ep.clone());

    let host_resends = Arc::new(AtomicBool::new(false));

    // Dedicated sync frame delivery channel so JNI/MediaCodec never stalls QUIC acks.
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Delivered>(8);
    let cb_frame = cb.clone();
    let cancel_frame = cancel.clone();
    std::thread::Builder::new()
        .name("rc-frame-cb".into())
        .spawn(move || {
            while let Ok(d) = frame_rx.recv() {
                if cancel_frame.load(Ordering::Relaxed) {
                    break;
                }
                let codec = if d.header.hevc { 1 } else { 0 };
                cb_frame.on_video_frame(
                    codec,
                    d.header.key,
                    d.header.width as i32,
                    d.header.height as i32,
                    d.header.host_us as i64,
                    &d.data,
                );
            }
        })
        .ok();

    let mut tasks = spawn_conn_tasks(&conn, send, recv, &ctrl_tx, &host_resends, &frame_tx);
    let mut connected_notified = false;

    loop {
        enum LoopEvent {
            Inbound(Option<Msg>),
            Outbound(Option<Msg>),
            SwitchRoute(Option<bool>),
        }

        let ev = tokio::select! {
            m = tasks.inbound_rx.recv() => LoopEvent::Inbound(m),
            m = ctrl_rx.recv() => LoopEvent::Outbound(m),
            r = route_rx.recv() => LoopEvent::SwitchRoute(r),
        };

        match ev {
            LoopEvent::Outbound(Some(msg)) => {
                let _ = tasks.out_tx.send(msg);
            }
            LoopEvent::Outbound(None) => break,
            LoopEvent::SwitchRoute(None) => break,
            LoopEvent::SwitchRoute(Some(mut want)) => {
                while let Ok(v) = route_rx.try_recv() {
                    want = v;
                }
                if want == cur_relay_only {
                    continue;
                }
                if want && !can_relay_only {
                    cb.on_route_changed(false);
                    cb.on_notice("当前没有可用的自建中转服务器");
                    continue;
                }
                cb.on_notice(if want {
                    "正在切换到只走中转…"
                } else {
                    "正在切换到自动线路…"
                });

                tasks.abort_all();
                conn.close(0u32.into(), b"route");
                let old_ep = ep;
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(2), old_ep.close()).await;
                });

                match connect_with_route(
                    &device_secret,
                    target,
                    &addrs,
                    &own_relays,
                    &extra_relays,
                    want,
                    &view_state,
                )
                .await
                {
                    Ok((new_ep, new_conn, new_send, new_recv)) => {
                        if cancel.load(Ordering::Relaxed) {
                            new_conn.close(0u32.into(), b"cancelled");
                            let _ = tokio::time::timeout(Duration::from_secs(1), new_ep.close()).await;
                            return;
                        }
                        ep = new_ep;
                        conn = new_conn;
                        cur_relay_only = want;
                        *conn_slot.lock().unwrap() = Some(conn.clone());
                        *ep_slot.lock().unwrap() = Some(ep.clone());
                        tasks = spawn_conn_tasks(&conn, new_send, new_recv, &ctrl_tx, &host_resends, &frame_tx);
                        cb.on_route_changed(want);
                        cb.on_notice(if want {
                            "已切到只走中转"
                        } else {
                            "已切到自动线路"
                        });
                    }
                    Err(_) => {
                        let fallback = !want;
                        cb.on_route_changed(fallback);
                        cb.on_notice("切换线路失败，正在恢复原线路…");
                        match connect_with_route(
                            &device_secret,
                            target,
                            &addrs,
                            &own_relays,
                            &extra_relays,
                            fallback,
                            &view_state,
                        )
                        .await
                        {
                            Ok((new_ep, new_conn, new_send, new_recv)) => {
                                ep = new_ep;
                                conn = new_conn;
                                cur_relay_only = fallback;
                                *conn_slot.lock().unwrap() = Some(conn.clone());
                                *ep_slot.lock().unwrap() = Some(ep.clone());
                                tasks = spawn_conn_tasks(
                                    &conn,
                                    new_send,
                                    new_recv,
                                    &ctrl_tx,
                                    &host_resends,
                                    &frame_tx,
                                );
                            }
                            Err(err2) => {
                                if !cancel.load(Ordering::Relaxed) {
                                    cb.on_session_state("disconnected", &err2);
                                }
                                return;
                            }
                        }
                    }
                }
            }
            LoopEvent::Inbound(None) => {
                tasks.abort_all();
                break;
            }
            LoopEvent::Inbound(Some(msg)) => match msg {
                Msg::Waiting => {
                    cb.on_session_state("waiting_consent", "等待对方在电脑上点击「允许」…");
                }
                Msg::Paused { on } => {
                    if on {
                        cb.on_session_state("paused", "当前仅通过公共中转，画面已暂停，等待直连…");
                    } else if connected_notified {
                        cb.on_session_state("connected", "");
                    }
                }
                Msg::Info {
                    width,
                    height,
                    encoder,
                    streaming,
                } => {
                    let vs = view_state.lock().unwrap().clone();
                    let fallback_fps = if vs.quality.fps == 0 { 60 } else { vs.quality.fps };
                    let (s_fps, s_kbps, s_resends) = match streaming {
                        Some(s) => (s.fps as i32, s.max_kbps as i32, s.resends),
                        None => (fallback_fps as i32, 20_000, false),
                    };
                    host_resends.store(s_resends, Ordering::Relaxed);
                    cb.on_stream_info(width as i32, height as i32, &encoder, s_fps, s_kbps);
                    if !connected_notified {
                        connected_notified = true;
                        cb.on_session_state("connected", "");
                    }
                }
                Msg::Monitors { list, current } => {
                    let items: Vec<MonitorJson<'_>> = list
                        .iter()
                        .enumerate()
                        .map(|(i, m)| MonitorJson {
                            index: i,
                            name: &m.name,
                            x: m.x,
                            y: m.y,
                            width: m.width,
                            height: m.height,
                            primary: m.primary,
                            rotation: m.rotation,
                        })
                        .collect();
                    if let Ok(json) = serde_json::to_string(&items) {
                        cb.on_monitors(&json, current as i32);
                    }
                }
                Msg::Windows { list } => {
                    let items: Vec<WindowJson<'_>> = list
                        .iter()
                        .map(|w| WindowJson {
                            hwnd: w.hwnd,
                            app_key: &w.app_key,
                            app_name: &w.app_name,
                            title: &w.title,
                            monitor: w.monitor,
                            minimized: w.minimized,
                            rx: w.rx as f32 / 65535.0,
                            ry: w.ry as f32 / 65535.0,
                            rw: w.rw as f32 / 65535.0,
                            rh: w.rh as f32 / 65535.0,
                        })
                        .collect();
                    if let Ok(json) = serde_json::to_string(&items) {
                        cb.on_windows(&json);
                    }
                }
                Msg::AppIcon { app_key, png } => {
                    if !png.is_empty() {
                        cb.on_app_icon(&app_key, &png);
                    }
                }
                Msg::WindowThumb { hwnd, png } => {
                    if !png.is_empty() {
                        cb.on_window_thumb(hwnd, &png);
                    }
                }
                Msg::Stats(s) => {
                    let (kind, rtt_ms) = net_util::selected_path(&conn);
                    let rtt = if s.path_rtt_us > 0 {
                        (s.path_rtt_us / 1000) as i32
                    } else {
                        rtt_ms.round() as i32
                    };
                    cb.on_stats(s.fps as i32, s.kbps as i32, rtt, kind.as_str());
                }
                Msg::Hint { what } => {
                    cb.on_hint(what as i32, hint_text(what));
                }
                Msg::CursorShape { id, image } => {
                    if let Some(rgba) = cursor_to_rgba(&image) {
                        cb.on_cursor_shape(
                            id as i32,
                            image.width as i32,
                            image.height as i32,
                            image.hot_x as i32,
                            image.hot_y as i32,
                            &rgba,
                        );
                    }
                }
                Msg::CursorUse { id } => {
                    cb.on_cursor_use(id as i32);
                }
                Msg::CursorHidden => {
                    cb.on_cursor_hidden();
                }
                Msg::CursorMeta { kind } => {
                    cb.on_cursor_meta(kind as i32);
                }
                Msg::TextFocus { editable } => {
                    cb.on_text_focus(editable);
                }
                Msg::VirtualDisplay {
                    active,
                    width,
                    height,
                    dpi_percent,
                } => {
                    cb.on_virtual_display(active, width as i32, height as i32, dpi_percent as i32);
                }
                Msg::Ping { t_us } => {
                    let _ = tasks.out_tx.send(Msg::Pong { t_us });
                }
                _ => {}
            },
        }
    }

    if !cancel.load(Ordering::Relaxed) {
        let reason = match conn.close_reason() {
            Some(err) => close_reason_text(&err),
            None => "连接已断开".into(),
        };
        cb.on_session_state("disconnected", &reason);
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
}

fn cursor_to_rgba(img: &CursorImage) -> Option<Vec<u8>> {
    if !img.valid() {
        return None;
    }
    let w = img.width as usize;
    let h = img.height as usize;
    let stride = CursorImage::stride(img.width);
    let mut out = vec![0u8; w * h * 4];

    let mask_bit = |row: usize, x: usize| -> bool {
        let byte = img.mask.get(row * stride + (x >> 3)).copied().unwrap_or(0);
        (byte & (0x80 >> (x & 7))) != 0
    };

    // Pixels Windows draws by inverting the screen (AND bit 1, XOR value non-zero). Android cannot
    // invert what is under an overlay, so they are drawn light with a dark halo instead.
    let mut invert_pts = vec![false; w * h];
    // Android copyPixelsFromBuffer expects premultiplied RGBA8888.
    const INVERT_PX: [u8; 4] = [245, 245, 248, 245];

    if img.is_color() {
        let has_alpha = img.color.chunks_exact(4).any(|p| p[3] != 0);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let (b, g, r, a) = (
                    img.color[i],
                    img.color[i + 1],
                    img.color[i + 2],
                    img.color[i + 3],
                );
                if has_alpha {
                    out[i] = r.min(a);
                    out[i + 1] = g.min(a);
                    out[i + 2] = b.min(a);
                    out[i + 3] = a;
                } else if !mask_bit(y, x) {
                    out[i..i + 4].copy_from_slice(&[r, g, b, 255]);
                } else if (r, g, b) != (0, 0, 0) {
                    // No alpha channel: the colour bitmap is the XOR mask, so AND 1 with a
                    // non-zero colour inverts (e.g. a monochrome I-beam Windows scaled to 32 bpp).
                    invert_pts[y * w + x] = true;
                    out[i..i + 4].copy_from_slice(&INVERT_PX);
                }
            }
        }
    } else {
        for y in 0..h {
            for x in 0..w {
                let and_b = mask_bit(y, x);
                let xor_b = mask_bit(h + y, x);
                let i = (y * w + x) * 4;
                match (and_b, xor_b) {
                    (false, false) => out[i..i + 4].copy_from_slice(&[0, 0, 0, 255]),
                    (false, true) => out[i..i + 4].copy_from_slice(&[255, 255, 255, 255]),
                    (true, false) => out[i..i + 4].copy_from_slice(&[0, 0, 0, 0]),
                    (true, true) => {
                        invert_pts[y * w + x] = true;
                        out[i..i + 4].copy_from_slice(&INVERT_PX);
                    }
                }
            }
        }
    }
    if invert_pts.iter().any(|&v| v) {
        // Add a subtle 1px dark halo around invert pixels (e.g. IDC_IBEAM)
        // so the cursor stays crisp over both bright white and dark backgrounds.
        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                if out[idx * 4 + 3] != 0 {
                    continue;
                }
                let touches_invert = (x > 0 && invert_pts[idx - 1])
                    || (x + 1 < w && invert_pts[idx + 1])
                    || (y > 0 && invert_pts[idx - w])
                    || (y + 1 < h && invert_pts[idx + w]);
                if touches_invert {
                    // Premultiplied dark outline (alpha 185).
                    out[idx * 4..idx * 4 + 4].copy_from_slice(&[14, 14, 18, 185]);
                }
            }
        }
    }
    Some(out)
}

async fn run_video_rx(
    conn: Connection,
    ctrl: mpsc::UnboundedSender<Msg>,
    host_resends: Arc<AtomicBool>,
    frame_tx: std::sync::mpsc::SyncSender<Delivered>,
) {
    let mut asm = Assembler::new(FRAME_LINGER);
    let mut ro = Reorder::default();
    let mut tick = tokio::time::interval(REPORT_EVERY);
    let mut check = tokio::time::interval(CHECK_EVERY);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut recent_acked = VecDeque::<u32>::with_capacity(64);
    let mut total = 0u64;
    let mut probe: Option<(u16, Instant, u64, Instant, u64)> = None;
    let mut probe_again: Option<ProbeResult> = None;

    loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let Ok(d) = d else { return };
                if matches!(d.first(), Some(&T_AUDIO | &T_AUDIO_RED)) {
                    total += d.len() as u64;
                    continue;
                }
                if d.first() == Some(&T_PROBE) && d.len() >= 3 {
                    total += d.len() as u64;
                    let (id, now) = (u16::from_be_bytes([d[1], d[2]]), Instant::now());
                    match probe.as_mut() {
                        Some(p) if p.0 == id => (p.3, p.4) = (now, total),
                        _ => {
                            if let Some(p) = probe.take() {
                                probe_again = send_probe_result(&conn, p);
                            }
                            probe = Some((id, now, total, now, total));
                        }
                    }
                    continue;
                }
                if !wire::is_shard(&d) {
                    continue;
                }
                total += d.len() as u64;
                let now = Instant::now();
                if let Some((h, _)) = ShardHeader::parse(&d) {
                    ro.on_shard(h.frame, now);
                }
                let Some(c) = asm.push(&d, now) else { continue };
                let mut prev_mask = 0u32;
                for &f in &recent_acked {
                    let diff = c.frame.wrapping_sub(f);
                    if (1..=32).contains(&diff) {
                        prev_mask |= 1u32 << (diff - 1);
                    }
                }
                if recent_acked.len() == 64 {
                    recent_acked.pop_front();
                }
                recent_acked.push_back(c.frame);
                let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(
                    &Ack {
                        frame: c.frame,
                        recovered: c.recovered,
                        resent: c.resent,
                        prev_mask,
                    }
                    .encode(),
                ));
                let Some((header, body)) = FrameHeader::parse(&c.data) else { continue };
                let delivered = Delivered {
                    header,
                    data: body.to_vec(),
                    complete_at: now,
                    frame: c.frame,
                };
                for f in ro.push(c.frame, delivered, now) {
                    let is_key = f.header.key;
                    match frame_tx.try_send(f) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(f)) => {
                            if is_key {
                                let _ = frame_tx.send(f);
                            }
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
                    }
                }
            }
            _ = check.tick() => {
                let now = Instant::now();
                ro.resend = host_resends.load(Ordering::Relaxed);
                for f in ro.resends(now) {
                    if let Some(shards) = asm.wanted(f) {
                        let _ = conn.send_datagram(Resend { frame: f, shards }.encode().into());
                    }
                }
                if ro.tick(now) {
                    let _ = ctrl.send(Msg::RequestKeyframe);
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let (_, rtt_ms) = net_util::selected_path(&conn);
                ro.rtt = Duration::from_secs_f64(rtt_ms.max(0.0) / 1000.0);
                asm.set_linger(FRAME_LINGER.max(ro.rtt * 2 + RESEND_SLACK));
                asm.expire(now);
                let r = asm.take_report();
                if r.shards_expected > 0 {
                    let _ = conn.send_datagram(r.encode().into());
                }
                if let Some(r) = probe_again.take() {
                    let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(&r.encode()));
                }
                if let Some(p) = probe.take_if(|p| now.duration_since(p.3) >= PROBE_DONE) {
                    probe_again = send_probe_result(&conn, p);
                }
            }
        }
    }
}

fn send_probe_result(
    conn: &Connection,
    p: (u16, Instant, u64, Instant, u64),
) -> Option<ProbeResult> {
    let (id, first, first_total, last, last_total) = p;
    let span = last.duration_since(first);
    if span.is_zero() {
        return None;
    }
    let r = ProbeResult {
        id,
        bytes: (last_total - first_total).min(u32::MAX as u64) as u32,
        span_us: span.as_micros().min(u32::MAX as u128) as u32,
    };
    let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(&r.encode()));
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 px wide image (one 2-byte mask row each) with the AND bits of `and_row` on every row.
    fn color_img(and_row: u16, pixels: &[[u8; 4]]) -> CursorImage {
        let h = 1u16;
        let mut color = Vec::new();
        for x in 0..16 {
            color.extend_from_slice(pixels.get(x).unwrap_or(&[0, 0, 0, 0]));
        }
        CursorImage { width: 16, height: h, hot_x: 0, hot_y: 0, color, mask: and_row.to_be_bytes().to_vec() }
    }

    fn px(out: &[u8], x: usize) -> [u8; 4] {
        out[x * 4..x * 4 + 4].try_into().unwrap()
    }

    #[test]
    fn color_without_alpha_uses_and_mask() {
        // x0: AND 0, red -> opaque red. x1: AND 1, black -> transparent.
        // x2: AND 1, white (XOR invert) -> visible invert pixel, not transparent.
        let img = color_img(0b0111_1111_1111_1111, &[[0, 0, 255, 0], [0, 0, 0, 0], [255, 255, 255, 0]]);
        let out = cursor_to_rgba(&img).unwrap();
        assert_eq!(px(&out, 0), [255, 0, 0, 255]);
        assert_eq!(px(&out, 2), [245, 245, 248, 245]);
        // x1 sits next to the invert pixel, so it gets the dark halo; x4 stays clear.
        assert_eq!(px(&out, 1)[3], 185);
        assert_eq!(px(&out, 4), [0, 0, 0, 0]);
    }

    #[test]
    fn color_with_alpha_is_premultiplied() {
        let img = color_img(0xFFFF, &[[200, 100, 50, 128]]);
        let out = cursor_to_rgba(&img).unwrap();
        assert_eq!(px(&out, 0), [50, 100, 128, 128]);
        assert_eq!(px(&out, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn monochrome_xor_pixels_stay_visible() {
        // Rows: AND then XOR. x0 black, x1 white, x2 screen (AND 1, XOR 0), x3 invert.
        let and_row: u16 = 0b0011_1111_1111_1111;
        let xor_row: u16 = 0b0101_0000_0000_0000;
        let mut mask = and_row.to_be_bytes().to_vec();
        mask.extend_from_slice(&xor_row.to_be_bytes());
        let img = CursorImage { width: 16, height: 1, hot_x: 0, hot_y: 0, color: vec![], mask };
        let out = cursor_to_rgba(&img).unwrap();
        assert_eq!(px(&out, 0), [0, 0, 0, 255]);
        assert_eq!(px(&out, 1), [255, 255, 255, 255]);
        assert_eq!(px(&out, 3), [245, 245, 248, 245]);
        // x2 and x4 border the invert pixel and get the halo; x6 stays transparent.
        assert_eq!(px(&out, 2)[3], 185);
        assert_eq!(px(&out, 4)[3], 185);
        assert_eq!(px(&out, 6), [0, 0, 0, 0]);
    }
}
