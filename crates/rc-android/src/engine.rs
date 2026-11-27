//! Persistent mobile controller engine: LAN discovery, `rc/meet/1` handshakes,
//! connection codes (`rc1...`), self-hosted relay codes (`rcr1...`), and desktop sessions.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use iroh::{Endpoint, EndpointId, RelayUrl, SecretKey, endpoint::Connection};
use rc_ident::{
    conn_code::{ConnCode, Find},
    relay_code::RelayCode,
};
use rc_proto::{
    discovery,
    proto::{
        EVENT_ASK, EVENT_FORGET, EVENT_NO, EVENT_NONE, EVENT_REMEMBER, MEET_ALPN, Msg,
        PLATFORM_ANDROID, REFUSE_CODE_EXPIRED, REFUSE_CODE_UNKNOWN, REFUSE_CODE_USED,
        REFUSE_FORGOTTEN, read_msg,
    },
};
use rc_relay::api::{Guest, Kind as GuestKind};
use serde::Serialize;
use tokio::{runtime::Runtime, sync::Notify};

use crate::{
    net_util::{self, PathKind},
    session::{ActiveSession, Callback},
    store::Store,
};

const LAN_SCAN_INTERVAL: Duration = Duration::from_millis(2500);
const LAN_SCAN_WAIT: Duration = Duration::from_millis(700);
const LAN_FORGET: Duration = Duration::from_secs(12);
const MEET_TIMEOUT: Duration = Duration::from_secs(6);
const CODE_NET_TIMEOUT: Duration = Duration::from_secs(12);

/// Hot-swappable callback proxy so recreated Android ViewModels seamlessly take over
/// notifications from the long-lived native Engine singleton.
struct DynamicCallback {
    inner: RwLock<Arc<dyn Callback>>,
}

impl DynamicCallback {
    fn new(cb: Arc<dyn Callback>) -> Self {
        Self {
            inner: RwLock::new(cb),
        }
    }

    fn replace(&self, cb: Arc<dyn Callback>) {
        let _old = self
            .inner
            .write()
            .ok()
            .map(|mut guard| std::mem::replace(&mut *guard, cb));
    }

    fn current(&self) -> Arc<dyn Callback> {
        self.inner.read().unwrap().clone()
    }
}

impl Callback for DynamicCallback {
    fn on_devices_updated(&self, devices_json: &str) {
        self.current().on_devices_updated(devices_json);
    }

    fn on_session_state(&self, state: &str, message: &str) {
        self.current().on_session_state(state, message);
    }

    fn on_stream_info(&self, width: i32, height: i32, encoder: &str, fps: i32, max_kbps: i32) {
        self.current()
            .on_stream_info(width, height, encoder, fps, max_kbps);
    }

    fn on_monitors(&self, monitors_json: &str, current_index: i32) {
        self.current().on_monitors(monitors_json, current_index);
    }

    fn on_windows(&self, windows_json: &str) {
        self.current().on_windows(windows_json);
    }

    fn on_app_icon(&self, app_key: &str, png: &[u8]) {
        self.current().on_app_icon(app_key, png);
    }

    fn on_window_thumb(&self, hwnd: u64, png: &[u8]) {
        self.current().on_window_thumb(hwnd, png);
    }

    fn on_stats(&self, fps: i32, kbps: i32, rtt_ms: i32, path_kind: &str) {
        self.current().on_stats(fps, kbps, rtt_ms, path_kind);
    }

    fn on_hint(&self, code: i32, message: &str) {
        self.current().on_hint(code, message);
    }

    fn on_notice(&self, message: &str) {
        self.current().on_notice(message);
    }

    fn on_cursor_shape(&self, id: i32, width: i32, height: i32, hot_x: i32, hot_y: i32, rgba: &[u8]) {
        self.current()
            .on_cursor_shape(id, width, height, hot_x, hot_y, rgba);
    }

    fn on_cursor_use(&self, id: i32) {
        self.current().on_cursor_use(id);
    }

    fn on_cursor_hidden(&self) {
        self.current().on_cursor_hidden();
    }

    fn on_cursor_meta(&self, kind: i32) {
        self.current().on_cursor_meta(kind);
    }

    fn on_text_focus(&self, editable: bool) {
        self.current().on_text_focus(editable);
    }

    fn on_virtual_display(&self, active: bool, width: i32, height: i32, dpi_percent: i32) {
        self.current()
            .on_virtual_display(active, width, height, dpi_percent);
    }

    fn on_route_changed(&self, relay_only: bool) {
        self.current().on_route_changed(relay_only);
    }

    fn on_video_frame(
        &self,
        codec: i32,
        key: bool,
        width: i32,
        height: i32,
        host_us: i64,
        data: &[u8],
    ) {
        self.current()
            .on_video_frame(codec, key, width, height, host_us, data);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    None,
    In,
    Out,
}

impl Ask {
    fn as_str(self) -> &'static str {
        match self {
            Ask::None => "none",
            Ask::In => "in",
            Ask::Out => "out",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PeerStatus {
    Checking,
    Online { rtt_ms: i32, path: PathKind },
    Offline,
    Refused(String),
}

struct PeerState {
    name: String,
    lan: Option<(SocketAddr, Instant)>,
    saved_addrs: Vec<SocketAddr>,
    code_addrs: Vec<SocketAddr>,
    token: Option<[u8; 8]>,
    invite_relays: Vec<RelayUrl>,
    home_relays: Vec<RelayUrl>,
    reached_at: Option<SocketAddr>,
    remembered: bool,
    tell_remember: bool,
    ask: Ask,
    say_no: bool,
    status: PeerStatus,
    conn: Option<Connection>,
    wake: Arc<Notify>,
    running: bool,
}

impl PeerState {
    fn new() -> Self {
        Self {
            name: String::new(),
            lan: None,
            saved_addrs: Vec::new(),
            code_addrs: Vec::new(),
            token: None,
            invite_relays: Vec::new(),
            home_relays: Vec::new(),
            reached_at: None,
            remembered: false,
            tell_remember: false,
            ask: Ask::None,
            say_no: false,
            status: PeerStatus::Checking,
            conn: None,
            wake: Arc::new(Notify::new()),
            running: false,
        }
    }

    fn lan_addr(&self) -> Option<SocketAddr> {
        self.lan
            .filter(|(_, t)| t.elapsed() < LAN_FORGET)
            .map(|(a, _)| a)
    }

    fn candidate_addrs(&self) -> Vec<SocketAddr> {
        let mut out = Vec::new();
        let mut push = |a: SocketAddr| {
            if !out.contains(&a) {
                out.push(a);
            }
        };
        if let Some(a) = self.lan_addr() {
            push(a);
        }
        if let Some(a) = self.reached_at {
            push(a);
        }
        for &a in &self.code_addrs {
            push(a);
        }
        for &a in &self.saved_addrs {
            push(a);
        }
        out
    }

    fn candidate_relays(&self) -> Vec<RelayUrl> {
        let mut out = Vec::new();
        let mut push = |u: &RelayUrl| {
            if !out.contains(u) {
                out.push(u.clone());
            }
        };
        for u in &self.invite_relays {
            push(u);
        }
        for u in &self.home_relays {
            push(u);
        }
        out
    }
}

#[derive(Serialize)]
struct DeviceItemJson {
    id: String,
    short_id: String,
    name: String,
    status: String,
    rtt_ms: i32,
    path: String,
    on_lan: bool,
    remembered: bool,
    has_self_hosted_relay: bool,
    ask: String,
    error: String,
}

#[derive(Serialize)]
struct DevicesPayloadJson {
    device_id: String,
    own_relay_count: usize,
    devices: Vec<DeviceItemJson>,
}

#[derive(Serialize)]
pub struct PasteResult {
    pub ok: bool,
    pub kind: String,
    pub message: String,
    pub device_id: Option<String>,
}

struct Inner {
    secret: SecretKey,
    me: EndpointId,
    device_name: Mutex<String>,
    store: Mutex<Store>,
    endpoint: Mutex<Option<Endpoint>>,
    peers: Mutex<BTreeMap<EndpointId, PeerState>>,
    broadcasts: Mutex<Vec<Ipv4Addr>>,
    session: Mutex<Option<ActiveSession>>,
    cb: Arc<DynamicCallback>,
}

pub struct Engine {
    rt: Runtime,
    inner: Arc<Inner>,
}

fn refusal_text(reason: u8) -> String {
    match reason {
        REFUSE_FORGOTTEN => "对方已把这台手机从设备列表中删除".into(),
        REFUSE_CODE_EXPIRED => "连接码已过期（超过 1 小时），请在电脑重新复制".into(),
        REFUSE_CODE_USED => "连接码已被其他设备使用，请在电脑重新复制".into(),
        REFUSE_CODE_UNKNOWN => "连接码无效或对方软件已重启，请重新复制".into(),
        _ => "对方尚未认识这台手机".into(),
    }
}

impl Engine {
    pub fn start(files_dir: PathBuf, device_name: String, cb: Arc<dyn Callback>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name("rc-android-rt")
            .build()?;

        let (secret, store) = Store::load(&files_dir)?;
        let me = secret.public();
        let own_relays = store.own_relay_urls();

        let mut initial_peers = BTreeMap::new();
        for d in &store.data.devices {
            if let Ok(id) = d.id.parse::<EndpointId>() {
                let mut ps = PeerState::new();
                ps.name = d.name.clone();
                ps.saved_addrs = d
                    .addrs
                    .iter()
                    .filter_map(|s| s.parse::<SocketAddr>().ok())
                    .collect();
                ps.home_relays = d
                    .home_relays
                    .iter()
                    .filter_map(|s| s.parse::<RelayUrl>().ok())
                    .collect();
                ps.remembered = d.remembered;
                ps.tell_remember = d.tell_remember;
                initial_peers.insert(id, ps);
            }
        }

        let extra_relays: Vec<RelayUrl> = initial_peers
            .values()
            .flat_map(|p| p.home_relays.iter().cloned())
            .collect();

        let ep = rt.block_on(net_util::create_endpoint(
            secret.clone(),
            &own_relays,
            &extra_relays,
        ))?;

        let inner = Arc::new(Inner {
            secret,
            me,
            device_name: Mutex::new(device_name),
            store: Mutex::new(store),
            endpoint: Mutex::new(Some(ep.clone())),
            peers: Mutex::new(initial_peers),
            broadcasts: Mutex::new(Vec::new()),
            session: Mutex::new(None),
            cb: Arc::new(DynamicCallback::new(cb)),
        });

        spawn_accept_loop(&rt, inner.clone(), ep);

        // Start peer loops for already-saved computers.
        let ids: Vec<EndpointId> = inner.peers.lock().unwrap().keys().copied().collect();
        for id in ids {
            ensure_peer_loop(&rt, inner.clone(), id);
        }

        // Start background LAN discovery loop.
        let lan_inner = inner.clone();
        rt.spawn(async move {
            lan_discovery_loop(lan_inner).await;
        });

        emit_devices(&inner);

        Ok(Self { rt, inner })
    }

    pub fn replace_callback(&self, cb: Arc<dyn Callback>) {
        self.inner.cb.replace(cb);
        emit_devices(&self.inner);
        self.trigger_scan();
    }

    pub fn set_broadcast_addrs(&self, addrs: Vec<Ipv4Addr>) {
        *self.inner.broadcasts.lock().unwrap() = addrs;
    }

    pub fn trigger_scan(&self) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            scan_once(&inner).await;
        });
    }

    pub fn paste_code(&self, text: &str) -> PasteResult {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return PasteResult {
                ok: false,
                kind: "none".into(),
                message: "请先粘贴连接码或自建中转码".into(),
                device_id: None,
            };
        }

        // 1. Self-hosted relay code (`rcr1...`)
        if let Some(rc) = RelayCode::find(trimmed) {
            let inner = self.inner.clone();
            let my_name = inner.device_name.lock().unwrap().clone();
            let res = self.rt.block_on(async move {
                let urls = tokio::time::timeout(
                    CODE_NET_TIMEOUT,
                    rc_relay::client::claim(
                        &rc,
                        &inner.secret,
                        &my_name,
                        &rc_relay::tls::ca(),
                    ),
                )
                .await
                .map_err(|_| anyhow!("连接自建中转服务器超时"))??;
                {
                    let mut st = inner.store.lock().unwrap();
                    st.add_own_relay(&urls);
                }
                rebuild_endpoint(&inner).await?;
                sync_own_relay_guests(&inner).await;
                emit_devices(&inner);
                Ok::<usize, anyhow::Error>(urls.len())
            });
            return match res {
                Ok(_) => PasteResult {
                    ok: true,
                    kind: "relay".into(),
                    message: "已加入自建中转服务器".into(),
                    device_id: None,
                },
                Err(e) => PasteResult {
                    ok: false,
                    kind: "relay".into(),
                    message: format!("加入自建中转失败: {e:#}"),
                    device_id: None,
                },
            };
        }

        // 2. Computer connection code (`rc1...`)
        if let Some((cc, _)) = ConnCode::find(trimmed) {
            if cc.device == self.inner.me {
                return PasteResult {
                    ok: false,
                    kind: "conn".into(),
                    message: "这是当前手机自己的连接码".into(),
                    device_id: None,
                };
            }
            let inner = self.inner.clone();
            let target = cc.device;
            let my_name = inner.device_name.lock().unwrap().clone();
            let res = self.rt.block_on(async move {
                let mut invite_relays = Vec::new();
                let mut home_relays = Vec::new();
                match &cc.find {
                    Find::Relay(urls) => {
                        rc_relay::tls::learn_all(urls);
                        tokio::time::timeout(
                            CODE_NET_TIMEOUT,
                            rc_relay::client::meet(
                                urls,
                                &inner.secret,
                                &my_name,
                                &rc_relay::tls::ca(),
                                cc.device,
                                cc.token,
                            ),
                        )
                        .await
                        .map_err(|_| anyhow!("中转握手超时，请检查网络"))??;
                        invite_relays = urls.clone();
                        home_relays = urls.clone();
                    }
                    Find::Public(n) => {
                        if let Some(u) = net_util::public_relay(*n) {
                            home_relays.push(u);
                        }
                    }
                    Find::Nowhere => {}
                }

                let wake = {
                    let mut peers = inner.peers.lock().unwrap();
                    let p = peers.entry(target).or_insert_with(PeerState::new);
                    p.code_addrs = cc.addrs.clone();
                    p.token = Some(cc.token);
                    if !invite_relays.is_empty() {
                        p.invite_relays = invite_relays.clone();
                    }
                    if !home_relays.is_empty() {
                        p.home_relays = home_relays.clone();
                    }
                    p.status = PeerStatus::Checking;
                    p.ask = Ask::None;
                    p.say_no = false;
                    if let Some(old) = p.conn.take() {
                        old.close(0u32.into(), b"redial with code");
                    }
                    p.wake.clone()
                };

                if !invite_relays.is_empty() {
                    let _ = rebuild_endpoint(&inner).await;
                }
                {
                    let mut st = inner.store.lock().unwrap();
                    st.upsert_device(
                        &target,
                        None,
                        cc.addrs.first().copied(),
                        &home_relays,
                        None,
                    );
                }
                emit_devices(&inner);
                wake.notify_one();
                Ok::<(), anyhow::Error>(())
            });
            ensure_peer_loop(&self.rt, self.inner.clone(), target);
            return match res {
                Ok(()) => PasteResult {
                    ok: true,
                    kind: "conn".into(),
                    message: "连接码已识别，正在与对方握手…".into(),
                    device_id: Some(target.to_string()),
                },
                Err(e) => PasteResult {
                    ok: false,
                    kind: "conn".into(),
                    message: format!("连接码处理失败: {e:#}"),
                    device_id: None,
                },
            };
        }

        PasteResult {
            ok: false,
            kind: "unknown".into(),
            message: "未识别到有效的连接码（rc1...）或自建中转码（rcr1...）".into(),
            device_id: None,
        }
    }

    pub fn connect(
        &self,
        id_hex: &str,
        fit_w: u16,
        fit_h: u16,
        fps: u8,
        relay_only: bool,
    ) -> Result<()> {
        let target: EndpointId = id_hex
            .trim()
            .parse()
            .map_err(|_| anyhow!("无效的设备 ID"))?;
        self.disconnect();

        let own_relays = self.inner.store.lock().unwrap().own_relay_urls();
        let (addrs, extra_relays) = {
            let peers = self.inner.peers.lock().unwrap();
            match peers.get(&target) {
                Some(p) => (p.candidate_addrs(), p.candidate_relays()),
                None => (Vec::new(), Vec::new()),
            }
        };

        let _guard = self.rt.enter();
        let sess = ActiveSession::start(
            self.inner.secret.clone(),
            target,
            addrs,
            own_relays,
            extra_relays,
            fit_w,
            fit_h,
            fps,
            relay_only,
            self.inner.cb.clone(),
        );
        *self.inner.session.lock().unwrap() = Some(sess);
        Ok(())
    }

    pub fn set_relay_only(&self, relay_only: bool) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.set_relay_only(relay_only);
        }
    }

    pub fn disconnect(&self) {
        // Called from a Java thread: closing spawns the endpoint shutdown on the runtime.
        let _guard = self.rt.enter();
        if let Some(sess) = self.inner.session.lock().unwrap().take() {
            sess.close();
        }
    }

    pub fn select_monitor(&self, index: u8) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send(Msg::SelectMonitor { index });
        }
    }

    pub fn focus_window(&self, hwnd: u64, vw: u16, vh: u16) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send(Msg::FocusWindow { hwnd, vw, vh });
        }
    }

    pub fn send_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::TextInput {
                text: text.to_string(),
            });
        }
    }

    pub fn request_keyframe(&self) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send(Msg::RequestKeyframe);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_quality(
        &self,
        fit_w: u16,
        fit_h: u16,
        short: u16,
        fps: u8,
        kbps: u32,
        yuv444: bool,
        auto_fps: bool,
    ) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send(Msg::Quality(rc_proto::proto::Quality {
                fit_w,
                fit_h,
                short,
                fps,
                kbps,
                yuv444,
                auto_fps,
            }));
        }
    }

    pub fn send_mouse_move(&self, x: u16, y: u16) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::MouseMove { x, y });
        }
    }

    pub fn send_mouse_button(&self, button: u8, down: bool) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::MouseButton { button, down });
        }
    }

    pub fn send_touch(&self, id: u8, phase: u8, x: u16, y: u16) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::Touch { id, phase, x, y });
        }
    }

    pub fn send_touchpad(&self, phase: u8, dx: i16, dy: i16, scale_milli: u16) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::Touchpad {
                phase,
                dx,
                dy,
                scale_milli,
            });
        }
    }

    pub fn send_pen(&self, phase: u8, x: u16, y: u16, pressure: u16, tilt_x: i8, tilt_y: i8, flags: u8) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::Pen {
                phase,
                x,
                y,
                pressure,
                tilt_x,
                tilt_y,
                flags,
            });
        }
    }

    pub fn send_mouse_delta(&self, dx: i16, dy: i16) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::MouseDelta { dx, dy });
        }
    }

    pub fn set_virtual_display(&self, active: bool, width: u16, height: u16, dpi_percent: u16) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send(Msg::VirtualDisplay {
                active,
                width,
                height,
                dpi_percent,
            });
        }
    }

    pub fn send_wheel(&self, delta: i16, horizontal: bool) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::Wheel { delta, horizontal });
        }
    }

    pub fn send_key(&self, scan: u16, down: bool) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send_input(Msg::Key { scan, down });
        }
    }

    pub fn send_release_all(&self) {
        if let Some(s) = self.inner.session.lock().unwrap().as_ref() {
            s.send(Msg::ReleaseAll);
        }
    }

    pub fn remember_device(&self, id_hex: &str) {
        let Ok(target) = id_hex.trim().parse::<EndpointId>() else {
            return;
        };
        enum Act {
            None,
            Both,
            Asked(Arc<Notify>),
        }
        let act = {
            let mut st = self.inner.store.lock().unwrap();
            if st.is_remembered(&target) {
                Act::None
            } else {
                st.clear_forgotten(&target);
                let mut peers = self.inner.peers.lock().unwrap();
                match peers.get_mut(&target) {
                    Some(p) if p.ask == Ask::In => {
                        drop(peers);
                        drop(st);
                        befriend(&self.inner, target, true);
                        Act::Both
                    }
                    Some(p) => {
                        p.ask = Ask::Out;
                        p.say_no = false;
                        Act::Asked(p.wake.clone())
                    }
                    None => Act::None,
                }
            }
        };
        match act {
            Act::Both => {
                let name = shown_name(&self.inner, &target);
                self.inner.cb.on_notice(&format!("已和「{name}」互相记住"));
                emit_devices(&self.inner);
            }
            Act::Asked(wake) => {
                wake.notify_one();
                emit_devices(&self.inner);
            }
            Act::None => {}
        }
    }

    pub fn agree_remember(&self, id_hex: &str) {
        let Ok(target) = id_hex.trim().parse::<EndpointId>() else {
            return;
        };
        let has_in = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(&target)
            .is_some_and(|p| p.ask == Ask::In);
        if has_in {
            befriend(&self.inner, target, true);
            let name = shown_name(&self.inner, &target);
            self.inner.cb.on_notice(&format!("已和「{name}」互相记住"));
            emit_devices(&self.inner);
        }
    }

    pub fn refuse_remember(&self, id_hex: &str) {
        let Ok(target) = id_hex.trim().parse::<EndpointId>() else {
            return;
        };
        let wake = {
            let mut peers = self.inner.peers.lock().unwrap();
            peers
                .get_mut(&target)
                .filter(|p| p.ask == Ask::In)
                .map(|p| {
                    p.ask = Ask::None;
                    p.say_no = true;
                    p.wake.clone()
                })
        };
        if let Some(w) = wake {
            w.notify_one();
            emit_devices(&self.inner);
        }
    }

    pub fn cancel_remember(&self, id_hex: &str) {
        let Ok(target) = id_hex.trim().parse::<EndpointId>() else {
            return;
        };
        let wake = {
            let mut peers = self.inner.peers.lock().unwrap();
            peers
                .get_mut(&target)
                .filter(|p| p.ask == Ask::Out)
                .map(|p| {
                    p.ask = Ask::None;
                    p.say_no = true;
                    p.wake.clone()
                })
        };
        if let Some(w) = wake {
            w.notify_one();
            emit_devices(&self.inner);
        }
    }

    pub fn retry_device(&self, id_hex: &str) {
        let Ok(target) = id_hex.trim().parse::<EndpointId>() else {
            return;
        };
        let wake = {
            let mut peers = self.inner.peers.lock().unwrap();
            peers.get_mut(&target).map(|p| {
                if let Some(c) = p.conn.take() {
                    c.close(0u32.into(), b"retry");
                }
                p.status = PeerStatus::Checking;
                p.wake.clone()
            })
        };
        if let Some(w) = wake {
            w.notify_one();
            emit_devices(&self.inner);
        }
    }

    /// Rename this phone. Computers learn names during the meet handshake, so every
    /// live meet connection is redialed to show the new name right away.
    pub fn set_device_name(&self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        {
            let mut cur = self.inner.device_name.lock().unwrap();
            if *cur == name {
                return;
            }
            *cur = name.to_string();
        }
        let wakes: Vec<_> = {
            let mut peers = self.inner.peers.lock().unwrap();
            peers
                .values_mut()
                .filter_map(|p| {
                    let c = p.conn.take()?;
                    c.close(0u32.into(), b"rename");
                    Some(p.wake.clone())
                })
                .collect()
        };
        for w in wakes {
            w.notify_one();
        }
    }

    pub fn forget_device(&self, id_hex: &str) {
        let Ok(target) = id_hex.trim().parse::<EndpointId>() else {
            return;
        };
        let was_remembered = {
            let mut st = self.inner.store.lock().unwrap();
            let was_rem = st.is_remembered(&target);
            if was_rem {
                st.mark_forgotten(&target);
            } else {
                st.remove_device(&target);
            }
            was_rem
        };
        let wake = {
            let mut peers = self.inner.peers.lock().unwrap();
            if was_remembered {
                peers.get_mut(&target).map(|p| {
                    p.remembered = false;
                    p.tell_remember = false;
                    p.ask = Ask::None;
                    p.say_no = false;
                    p.wake.clone()
                })
            } else {
                if let Some(p) = peers.get_mut(&target) {
                    p.remembered = false;
                    p.tell_remember = false;
                    p.ask = Ask::None;
                    p.say_no = false;
                    p.code_addrs.clear();
                    p.saved_addrs.clear();
                    p.home_relays.clear();
                    p.invite_relays.clear();
                    p.token = None;
                    if p.lan_addr().is_none() {
                        if let Some(c) = p.conn.take() {
                            c.close(0u32.into(), b"removed");
                        }
                        peers.remove(&target);
                    }
                }
                None
            }
        };
        if let Some(w) = wake {
            w.notify_one();
        }
        emit_devices(&self.inner);
    }
}

async fn rebuild_endpoint(inner: &Arc<Inner>) -> Result<()> {
    let own = inner.store.lock().unwrap().own_relay_urls();
    let extra: Vec<RelayUrl> = inner
        .peers
        .lock()
        .unwrap()
        .values()
        .flat_map(|p| p.candidate_relays())
        .collect();
    let new_ep = net_util::create_endpoint(inner.secret.clone(), &own, &extra).await?;
    let ep_for_accept = new_ep.clone();
    let old = inner.endpoint.lock().unwrap().replace(new_ep);
    if let Some(ep) = old {
        tokio::spawn(async move {
            ep.close().await;
        });
    }
    let inner_accept = inner.clone();
    tokio::spawn(async move {
        accept_loop(inner_accept, ep_for_accept).await;
    });
    let wakes: Vec<Arc<Notify>> = inner
        .peers
        .lock()
        .unwrap()
        .values()
        .map(|p| p.wake.clone())
        .collect();
    for w in wakes {
        w.notify_one();
    }
    Ok(())
}

async fn sync_own_relay_guests(inner: &Arc<Inner>) {
    let (relay_groups, guests) = {
        let st = inner.store.lock().unwrap();
        let groups: Vec<Vec<RelayUrl>> = st
            .data
            .own_relays
            .iter()
            .map(|g| {
                g.iter()
                    .filter_map(|s| s.parse::<RelayUrl>().ok())
                    .collect()
            })
            .filter(|g: &Vec<RelayUrl>| !g.is_empty())
            .collect();
        let guests: Vec<Guest> = st
            .data
            .devices
            .iter()
            .filter_map(|d| {
                let id = d.id.parse::<EndpointId>().ok()?;
                Some(Guest::new(id, d.name.clone(), GuestKind::Trusted))
            })
            .collect();
        (groups, guests)
    };
    let ca = rc_relay::tls::ca();
    for g in relay_groups {
        let _ = rc_relay::client::set_guests(&g, &inner.secret, &ca, &guests).await;
    }
}

async fn lan_discovery_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(LAN_SCAN_INTERVAL);
    loop {
        tick.tick().await;
        scan_once(&inner).await;
    }
}

async fn scan_once(inner: &Arc<Inner>) {
    let broadcasts = inner.broadcasts.lock().unwrap().clone();
    let found = match discovery::scan_targets(LAN_SCAN_WAIT, &inner.me, &broadcasts).await {
        Ok(v) => v,
        Err(_) => return,
    };
    let now = Instant::now();
    let mut new_ids = Vec::new();
    let mut changed = false;
    {
        let st = inner.store.lock().unwrap();
        let mut peers = inner.peers.lock().unwrap();
        for f in found {
            if st.is_forgotten(&f.id) {
                continue;
            }
            let p = peers.entry(f.id).or_insert_with(|| {
                changed = true;
                PeerState::new()
            });
            if p.name.is_empty() && !f.name.is_empty() {
                p.name = f.name.clone();
                changed = true;
            }
            let addr_changed = p.lan.is_none_or(|(a, _)| a != f.addr);
            p.lan = Some((f.addr, now));
            if addr_changed || matches!(p.status, PeerStatus::Offline | PeerStatus::Refused(_)) {
                if addr_changed {
                    changed = true;
                }
                p.wake.notify_one();
            }
            if !p.running {
                p.running = true;
                new_ids.push(f.id);
            }
        }
        // Expire stale LAN-only peers that are no longer online.
        for p in peers.values_mut() {
            if p.lan.is_some_and(|(_, t)| t.elapsed() >= LAN_FORGET) {
                p.lan = None;
                changed = true;
            }
        }
    }
    for id in new_ids {
        let inner_clone = inner.clone();
        tokio::spawn(async move {
            peer_loop(inner_clone, id).await;
        });
    }
    if changed {
        emit_devices(inner);
    }
}

fn spawn_accept_loop(rt: &Runtime, inner: Arc<Inner>, ep: Endpoint) {
    rt.spawn(async move {
        accept_loop(inner, ep).await;
    });
}

async fn accept_loop(inner: Arc<Inner>, ep: Endpoint) {
    while let Some(incoming) = ep.accept().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else {
                return;
            };
            if conn.alpn() == MEET_ALPN {
                serve_meet(inner, conn).await;
            } else {
                conn.close(0u32.into(), b"unsupported");
            }
        });
    }
}

async fn next_hi(conn: &Connection) -> Result<Option<((String, String, Option<[u8; 8]>, u8), iroh::endpoint::SendStream)>> {
    let (send, mut recv) = match conn.accept_bi().await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let msg = tokio::time::timeout(Duration::from_secs(5), read_msg(&mut recv)).await??;
    match msg {
        Msg::MeetHi {
            name,
            home,
            token,
            event,
            ..
        } => Ok(Some(((name, home, token, event), send))),
        _ => Err(anyhow!("unexpected meet message")),
    }
}

async fn serve_meet(inner: Arc<Inner>, conn: Connection) {
    let from = conn.remote_id();
    if from == inner.me {
        conn.close(0u32.into(), b"self");
        return;
    }
    loop {
        match next_hi(&conn).await {
            Ok(Some(((name, home, token, event), mut send))) => {
                let forget = event == EVENT_FORGET && token.is_none();
                let reply = on_hi(&inner, from, &conn, name, home, event, forget);
                if send.write_all(&reply.encode()).await.is_err() {
                    break;
                }
                let _ = send.finish();
                emit_devices(&inner);
                if forget {
                    break;
                }
            }
            _ => break,
        }
    }
    let mut changed = false;
    {
        let mut peers = inner.peers.lock().unwrap();
        if let Some(p) = peers.get_mut(&from)
            && p.conn
                .as_ref()
                .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            p.conn = None;
            p.status = PeerStatus::Offline;
            p.wake.notify_one();
            changed = true;
        }
    }
    if changed {
        emit_devices(&inner);
    }
}

fn home_str_for(inner: &Inner, ep: Option<&Endpoint>) -> String {
    let own = inner.store.lock().unwrap().own_relay_urls();
    if !own.is_empty() {
        net_util::url_list(&own)
    } else if let Some(ep) = ep {
        ep.addr()
            .relay_urls()
            .next()
            .map(ToString::to_string)
            .unwrap_or_default()
    } else {
        String::new()
    }
}

fn shown_name(inner: &Inner, id: &EndpointId) -> String {
    if let Some(p) = inner.peers.lock().unwrap().get(id)
        && !p.name.is_empty()
    {
        return p.name.clone();
    }
    id.to_string().chars().take(8).collect()
}

fn wish_for(inner: &Inner, id: &EndpointId) -> u8 {
    let st = inner.store.lock().unwrap();
    if st.is_forgotten(id) {
        return EVENT_FORGET;
    }
    let peers = inner.peers.lock().unwrap();
    let p = peers.get(id);
    if p.is_some_and(|p| p.tell_remember) {
        EVENT_REMEMBER
    } else if st.is_remembered(id) || p.is_some_and(|p| p.remembered) {
        EVENT_NONE
    } else if p.is_some_and(|p| p.say_no) {
        EVENT_NO
    } else if p.is_some_and(|p| p.ask == Ask::Out) {
        EVENT_ASK
    } else {
        EVENT_NONE
    }
}

fn befriend(inner: &Arc<Inner>, id: EndpointId, tell: bool) {
    let (name, direct, home) = {
        let mut peers = inner.peers.lock().unwrap();
        let p = peers.entry(id).or_insert_with(PeerState::new);
        p.remembered = true;
        p.tell_remember = tell;
        p.ask = Ask::None;
        p.say_no = false;
        (p.name.clone(), p.reached_at, p.home_relays.clone())
    };
    {
        let mut st = inner.store.lock().unwrap();
        st.upsert_device(&id, Some(&name), direct, &home, Some(true));
        st.set_tell_remember(&id, tell);
    }
    if let Some(w) = inner.peers.lock().unwrap().get(&id).map(|p| p.wake.clone()) {
        w.notify_one();
    }
}

fn forget_peer_local(inner: &Arc<Inner>, id: &EndpointId, notice: Option<String>) {
    {
        let mut st = inner.store.lock().unwrap();
        st.remove_device(id);
    }
    {
        let mut peers = inner.peers.lock().unwrap();
        if let Some(p) = peers.get_mut(id) {
            p.remembered = false;
            p.tell_remember = false;
            p.ask = Ask::None;
            p.say_no = false;
            p.code_addrs.clear();
            p.saved_addrs.clear();
            p.home_relays.clear();
            p.invite_relays.clear();
            p.token = None;
            if p.lan_addr().is_none() {
                if let Some(c) = p.conn.take() {
                    c.close(0u32.into(), b"forgotten");
                }
                peers.remove(id);
            }
        }
    }
    if let Some(msg) = notice {
        inner.cb.on_notice(&msg);
    }
}

fn heard(inner: &Arc<Inner>, from: EndpointId, event: u8) {
    let (listed, ask, refusing, name) = {
        let st = inner.store.lock().unwrap();
        let peers = inner.peers.lock().unwrap();
        let listed = st.is_remembered(&from) || peers.get(&from).is_some_and(|p| p.remembered);
        let ask = peers.get(&from).map_or(Ask::None, |p| p.ask);
        let refusing = peers.get(&from).is_some_and(|p| p.say_no);
        let name = peers
            .get(&from)
            .filter(|p| !p.name.is_empty())
            .map(|p| p.name.clone())
            .unwrap_or_else(|| from.to_string().chars().take(8).collect());
        (listed, ask, refusing, name)
    };
    match event {
        EVENT_ASK | EVENT_REMEMBER if !listed && refusing => {}
        EVENT_ASK if listed => {
            let mut need_wake = false;
            {
                let mut st = inner.store.lock().unwrap();
                let mut peers = inner.peers.lock().unwrap();
                if let Some(p) = peers.get_mut(&from)
                    && !p.tell_remember
                {
                    p.tell_remember = true;
                    need_wake = true;
                }
                st.set_tell_remember(&from, true);
            }
            if need_wake
                && let Some(w) = inner.peers.lock().unwrap().get(&from).map(|p| p.wake.clone())
            {
                w.notify_one();
            }
        }
        EVENT_ASK | EVENT_REMEMBER if !listed && ask == Ask::Out => {
            befriend(inner, from, event == EVENT_ASK);
            let msg = if event == EVENT_ASK {
                format!("已和「{name}」互相记住")
            } else {
                format!("「{name}」同意了，已互相记住")
            };
            inner.cb.on_notice(&msg);
        }
        EVENT_ASK | EVENT_REMEMBER if !listed && ask == Ask::None => {
            let mut changed = false;
            {
                let mut peers = inner.peers.lock().unwrap();
                if let Some(p) = peers.get_mut(&from)
                    && p.ask != Ask::In
                {
                    p.ask = Ask::In;
                    changed = true;
                }
            }
            if changed {
                inner.cb.on_notice(&format!("「{name}」想记住这台手机"));
            }
        }
        EVENT_NO => {
            if let Some(p) = inner.peers.lock().unwrap().get_mut(&from) {
                p.ask = Ask::None;
            }
            if ask == Ask::Out || listed {
                if listed {
                    let mut st = inner.store.lock().unwrap();
                    st.remove_device(&from);
                    if let Some(p) = inner.peers.lock().unwrap().get_mut(&from) {
                        p.remembered = false;
                        p.tell_remember = false;
                    }
                }
                inner.cb.on_notice(&format!("「{name}」没有同意"));
            }
        }
        EVENT_FORGET => {
            forget_peer_local(inner, &from, Some(format!("「{name}」把这台手机删除了")));
        }
        _ => {}
    }
}

fn on_hi(
    inner: &Arc<Inner>,
    from: EndpointId,
    conn: &Connection,
    name: String,
    home: String,
    event: u8,
    forget: bool,
) -> Msg {
    let ep = inner.endpoint.lock().unwrap().clone();
    let home_reply = home_str_for(inner, ep.as_ref());
    let shown = shown_name(inner, &from);

    if forget {
        let had = inner.store.lock().unwrap().is_remembered(&from);
        let notice = if had {
            Some(format!("「{shown}」把这台手机删除了"))
        } else {
            None
        };
        forget_peer_local(inner, &from, notice);
        return Msg::MeetWelcome {
            name: inner.device_name.lock().unwrap().clone(),
            home: home_reply,
            grant: Some(String::new()),
            event: EVENT_NONE,
            platform: PLATFORM_ANDROID,
        };
    }

    let parsed_home = net_util::parse_relay_urls(&home);
    let direct = net_util::direct_addr(conn);
    let (kind, rtt) = net_util::selected_path(conn);
    let mut spawn_id = None;
    {
        let mut peers = inner.peers.lock().unwrap();
        let p = peers.entry(from).or_insert_with(PeerState::new);
        if !name.is_empty() {
            p.name = name;
        }
        if !parsed_home.is_empty() {
            p.home_relays = parsed_home;
        }
        if direct.is_some() {
            p.reached_at = direct;
        }
        if p.conn.as_ref().is_none_or(|c| c.close_reason().is_some()) {
            p.conn = Some(conn.clone());
            p.status = PeerStatus::Online {
                rtt_ms: rtt.round() as i32,
                path: kind,
            };
        }
        if !p.running {
            p.running = true;
            spawn_id = Some(from);
        }
    }
    if let Some(id) = spawn_id {
        let inner_clone = inner.clone();
        tokio::spawn(async move {
            peer_loop(inner_clone, id).await;
        });
    }

    heard(inner, from, event);
    let mine = wish_for(inner, &from);
    if mine == EVENT_NO
        && let Some(p) = inner.peers.lock().unwrap().get_mut(&from)
    {
        p.say_no = false;
    }

    Msg::MeetWelcome {
        name: inner.device_name.lock().unwrap().clone(),
        home: home_reply,
        grant: Some(String::new()),
        event: mine,
        platform: PLATFORM_ANDROID,
    }
}

fn ensure_peer_loop(rt: &Runtime, inner: Arc<Inner>, id: EndpointId) {
    let should_spawn = {
        let mut peers = inner.peers.lock().unwrap();
        match peers.get_mut(&id) {
            Some(p) if !p.running => {
                p.running = true;
                true
            }
            _ => false,
        }
    };
    if should_spawn {
        rt.spawn(async move {
            peer_loop(inner, id).await;
        });
    }
}

async fn peer_loop(inner: Arc<Inner>, id: EndpointId) {
    let wake = {
        let peers = inner.peers.lock().unwrap();
        match peers.get(&id) {
            Some(p) => p.wake.clone(),
            None => return,
        }
    };

    loop {
        let Some(ep) = inner.endpoint.lock().unwrap().clone() else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };

        let (addrs, relays, token, existing_conn) = {
            let peers = inner.peers.lock().unwrap();
            let Some(p) = peers.get(&id) else { return };
            (
                p.candidate_addrs(),
                p.candidate_relays(),
                p.token,
                p.conn
                    .as_ref()
                    .filter(|c| c.close_reason().is_none())
                    .cloned(),
            )
        };
        let event = wish_for(&inner, &id);

        let conn = match existing_conn {
            Some(c) => c,
            None => {
                if addrs.is_empty() && relays.is_empty() {
                    if inner.store.lock().unwrap().is_forgotten(&id) {
                        let mut st = inner.store.lock().unwrap();
                        st.clear_forgotten(&id);
                        drop(st);
                        inner.peers.lock().unwrap().remove(&id);
                        return;
                    }
                    tokio::select! {
                        _ = wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                    continue;
                }
                let ea = net_util::endpoint_addr(id, &addrs, &relays);
                match tokio::time::timeout(MEET_TIMEOUT, ep.connect(ea, MEET_ALPN)).await {
                    Ok(Ok(c)) => c,
                    _ => {
                        {
                            let mut peers = inner.peers.lock().unwrap();
                            if let Some(p) = peers.get_mut(&id) {
                                p.status = PeerStatus::Offline;
                                p.conn = None;
                            }
                        }
                        emit_devices(&inner);
                        tokio::select! {
                            _ = wake.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                        }
                        continue;
                    }
                }
            }
        };

        let home_str = home_str_for(&inner, Some(&ep));

        let hi = Msg::MeetHi {
            name: inner.device_name.lock().unwrap().clone(),
            home: home_str,
            token,
            event,
            grant: Some(String::new()),
            platform: PLATFORM_ANDROID,
        };

        match meet_ask(&conn, &hi).await {
            Ok(Msg::MeetWelcome {
                name,
                home,
                event: reply_ev,
                ..
            }) => {
                if event == EVENT_FORGET {
                    {
                        let mut st = inner.store.lock().unwrap();
                        st.remove_device(&id);
                    }
                    let stay_on_lan = {
                        let mut peers = inner.peers.lock().unwrap();
                        if let Some(p) = peers.get_mut(&id) {
                            p.remembered = false;
                            p.tell_remember = false;
                            p.ask = Ask::None;
                            p.say_no = false;
                            p.conn = None;
                            p.lan_addr().is_some()
                        } else {
                            false
                        }
                    };
                    conn.close(0u32.into(), b"forgotten");
                    if !stay_on_lan {
                        inner.peers.lock().unwrap().remove(&id);
                        emit_devices(&inner);
                        return;
                    }
                    emit_devices(&inner);
                    continue;
                }

                let (path_kind, rtt_ms) = net_util::selected_path(&conn);
                let direct = net_util::direct_addr(&conn);
                let parsed_home = net_util::parse_relay_urls(&home);
                let is_remembered;
                let has_lan;
                {
                    let mut peers = inner.peers.lock().unwrap();
                    if let Some(p) = peers.get_mut(&id) {
                        if !name.is_empty() {
                            p.name = name.clone();
                        }
                        if !parsed_home.is_empty() {
                            p.home_relays = parsed_home.clone();
                        }
                        if direct.is_some() {
                            p.reached_at = direct;
                        }
                        p.token = None;
                        p.invite_relays.clear();
                        if event == EVENT_REMEMBER {
                            p.tell_remember = false;
                        }
                        if event == EVENT_NO {
                            p.say_no = false;
                        }
                        p.conn = Some(conn.clone());
                        p.status = PeerStatus::Online {
                            rtt_ms: rtt_ms.round() as i32,
                            path: path_kind,
                        };
                        is_remembered = p.remembered;
                        has_lan = p.lan_addr().is_some();
                    } else {
                        is_remembered = false;
                        has_lan = false;
                    }
                }
                if event == EVENT_REMEMBER {
                    inner.store.lock().unwrap().set_tell_remember(&id, false);
                }
                heard(&inner, id, reply_ev);
                {
                    let mut st = inner.store.lock().unwrap();
                    let in_store = st.data.devices.iter().any(|d| d.id == id.to_string());
                    if in_store && (!name.is_empty() || direct.is_some() || !parsed_home.is_empty()) {
                        st.upsert_device(&id, Some(&name), direct, &parsed_home, None);
                    }
                }
                emit_devices(&inner);

                let hello_rounds = if !is_remembered && has_lan { 2u32 } else { 4u32 };
                let mut rounds = 0u32;
                loop {
                    tokio::select! {
                        _ = conn.closed() => {
                            {
                                let mut peers = inner.peers.lock().unwrap();
                                if let Some(p) = peers.get_mut(&id) {
                                    p.conn = None;
                                    p.status = PeerStatus::Offline;
                                }
                            }
                            emit_devices(&inner);
                            break;
                        }
                        _ = wake.notified() => {
                            let need_rehello = wish_for(&inner, &id) != EVENT_NONE
                                || inner.peers.lock().unwrap().get(&id).is_none_or(|p| p.token.is_some() || p.conn.is_none());
                            if need_rehello {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(3)) => {
                            let (kind, rtt) = net_util::selected_path(&conn);
                            let mut changed = false;
                            if let Some(p) = inner.peers.lock().unwrap().get_mut(&id) {
                                let next = PeerStatus::Online { rtt_ms: rtt.round() as i32, path: kind };
                                if p.status != next {
                                    p.status = next;
                                    changed = true;
                                }
                            }
                            if changed {
                                emit_devices(&inner);
                            }
                            rounds += 1;
                            if rounds >= hello_rounds {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(Msg::MeetRefuse { reason }) => {
                if event == EVENT_FORGET {
                    forget_peer_local(&inner, &id, None);
                    emit_devices(&inner);
                    return;
                }
                if reason == REFUSE_FORGOTTEN {
                    let was_listed = inner.store.lock().unwrap().is_remembered(&id);
                    let notice = if was_listed {
                        let name = shown_name(&inner, &id);
                        Some(format!("「{name}」把这台手机删除了"))
                    } else {
                        None
                    };
                    forget_peer_local(&inner, &id, notice);
                }
                let text = refusal_text(reason);
                {
                    let mut peers = inner.peers.lock().unwrap();
                    if let Some(p) = peers.get_mut(&id) {
                        p.token = None;
                        p.conn = None;
                        p.ask = Ask::None;
                        p.status = PeerStatus::Refused(text);
                    }
                }
                emit_devices(&inner);
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(8)) => {}
                }
            }
            _ => {
                {
                    let mut peers = inner.peers.lock().unwrap();
                    if let Some(p) = peers.get_mut(&id) {
                        p.conn = None;
                        p.status = PeerStatus::Offline;
                    }
                }
                emit_devices(&inner);
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                }
            }
        }
    }
}

async fn meet_ask(conn: &Connection, hi: &Msg) -> Result<Msg> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&hi.encode()).await?;
    let _ = send.finish();
    let msg = tokio::time::timeout(MEET_TIMEOUT, read_msg(&mut recv)).await??;
    Ok(msg)
}

fn emit_devices(inner: &Arc<Inner>) {
    let (own_relay_count, own_relays) = {
        let st = inner.store.lock().unwrap();
        (st.data.own_relays.len(), st.own_relay_urls())
    };
    let mut list = Vec::new();
    {
        let st = inner.store.lock().unwrap();
        let peers = inner.peers.lock().unwrap();
        for (id, p) in peers.iter() {
            if st.is_forgotten(id) {
                continue;
            }
            let (status, rtt_ms, path, error) = match &p.status {
                PeerStatus::Checking => ("checking".into(), 0, "none".into(), String::new()),
                PeerStatus::Online { rtt_ms, path } => {
                    ("online".into(), *rtt_ms, path.as_str().into(), String::new())
                }
                PeerStatus::Offline => ("offline".into(), 0, "none".into(), String::new()),
                PeerStatus::Refused(e) => ("refused".into(), 0, "none".into(), e.clone()),
            };
            let id_str = id.to_string();
            let short_id = id_str.chars().take(8).collect::<String>();
            let display_name = if p.name.is_empty() {
                format!("电脑 {short_id}")
            } else {
                p.name.clone()
            };
            let has_self_hosted_relay =
                net_util::has_self_hosted_relay(&own_relays, &p.candidate_relays());
            list.push(DeviceItemJson {
                id: id_str,
                short_id,
                name: display_name,
                status,
                rtt_ms,
                path,
                on_lan: p.lan_addr().is_some(),
                remembered: p.remembered,
                has_self_hosted_relay,
                ask: p.ask.as_str().into(),
                error,
            });
        }
    }
    // Incoming asks first, then online, then LAN, then alphabetical.
    list.sort_by(|a, b| {
        let ask_rank = |x: &str| if x == "in" { 0 } else { 1 };
        let rank = |s: &str| match s {
            "online" => 0,
            "checking" => 1,
            _ => 2,
        };
        ask_rank(&a.ask)
            .cmp(&ask_rank(&b.ask))
            .then_with(|| rank(&a.status).cmp(&rank(&b.status)))
            .then_with(|| b.on_lan.cmp(&a.on_lan))
            .then_with(|| a.name.cmp(&b.name))
    });
    let payload = DevicesPayloadJson {
        device_id: inner.me.to_string(),
        own_relay_count,
        devices: list,
    };
    if let Ok(json) = serde_json::to_string(&payload) {
        inner.cb.on_devices_updated(&json);
    }
}
