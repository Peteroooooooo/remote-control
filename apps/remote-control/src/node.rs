//! The node: this computer on the network. It owns the one endpoint with
//! the device key and keeps everything about other devices: the device list,
//! the devices met this run (on the LAN or with a connection code),
//! connection codes, LAN discovery, and who may control this computer (with
//! the confirmation popup). Without the service the lobby runs it in its own
//! process; with the service the service's helper runs it and the lobby
//! talks to it over a pipe (`ipc`). Either way the lobby only uses
//! `request`: a line of text in, a text answer out.
//!
//! Each device with a card gets a task (`peer_loop`) that keeps a meet
//! connection to it, says hello, and keeps its status: online with the
//! round trip and the kind of way, offline, or "cannot get through" (only a
//! public relay after 10 s, so no video could flow).
//!
//! With an own relay, the node also keeps this computer's part of the
//! relay's roster: who may use the relay to reach it (`guests_loop`), and
//! its connection codes not used yet (invites). A code then points at the
//! own relay; a new device brings the code's token there first (`meet` in
//! `rc_relay::client`), and the relay lets the two reach each other until
//! this computer puts it on the guest list. The new device does not make
//! the relay its home. When no own relay answers for a while, the public
//! relays help instead, if allowed (`fallback_loop`).
//!
//! A computer with no own relay in use stays at the self-hosted relay of
//! each device online there (`keep_loop`), so that device can reach it
//! through its relay when no direct way works; the relay is still not its
//! home and not told to others.
//!
//! A relay is this computer's own once it joined it with one of the relay's
//! codes; nothing hands a relay on to other computers. Hellos and welcomes
//! still carry an empty `grant`, so an older version on the other side
//! drops a relay it was once given here.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use iroh::{Endpoint, EndpointId, RelayConfig, RelayUrl, SecretKey, Watcher, endpoint::Connection};
use rc_ident::{
    conn_code::{ConnCode, Find},
    relay_code::RelayCode,
};
use rc_relay::api::{Guest, Invite, Kind as GuestKind, Refusal};
use tokio::sync::Notify;

use crate::{
    consent::{self, Answer, Kind},
    ctlbar, desktop, discovery, host, ident, log, meet, net,
    net::PathKind,
    pipeline::PipeConfig,
    proto::{self, Msg, read_msg},
    store::{self, Settings, Store, clean, relay_urls, url_list},
    term,
};

pub struct Options {
    pub key: PathBuf,
    /// In the service's helper: the lobby window is another process that
    /// may be closed (see `lobby_open`).
    pub service: bool,
    /// Put in the device list as "my computer" (tests).
    pub allow: Vec<EndpointId>,
    /// Answer LAN discovery and look for devices on the LAN.
    pub lan: bool,
    pub pipe: PipeConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Met {
    Lan,
    Code,
}

/// A request to remember each other (`devices-and-relay.md` 3.3), until
/// the other side answers or one of the two windows closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    None,
    /// This computer asked; waiting for the other side.
    Out,
    /// The other side asked; waiting for this computer's user.
    In,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Checking,
    Online { rtt_ms: f64, path: PathKind },
    Offline,
    /// Only a public relay between the two: they meet, but no video can flow.
    Unreachable,
    Refused(u8),
}

struct Peer {
    /// What it calls itself.
    name: String,
    /// Met this run, so known while the lobby is open even if not listed.
    met: Option<Met>,
    lan: Option<(SocketAddr, Instant)>,
    code_addrs: Vec<SocketAddr>,
    /// The token of its connection code, which this computer redeems.
    token: Option<[u8; 8]>,
    /// The other side's own relay its connection code points at, where the
    /// token is still to be brought (see the module comment).
    invite: Option<Vec<RelayUrl>>,
    /// Where it said it is reachable: its home relay's addresses.
    home: Vec<RelayUrl>,
    /// The direct address it was last connected at, either way: tried too,
    /// e.g. after this computer's endpoint changed ports (`renew`).
    reached_at: Option<SocketAddr>,
    status: Status,
    /// When the current meet connection came up.
    since: Option<Instant>,
    conn: Option<Connection>,
    wake: Arc<Notify>,
    running: bool,
    ask: Ask,
    /// Its request was refused here; the next hello or welcome says so.
    say_no: bool,
    /// A second meet connection to it, over its self-hosted relay (`keep_loop`).
    keeper: Option<Keeper>,
    /// No new keeper before this: one is on its way, or the last one failed.
    keep_after: Option<Instant>,
    keep_wait: Duration,
    /// Bumped when a freshly pasted code supplies new addresses/relay/token so
    /// an in-flight dial or stale connection is dropped and redialed at once.
    redial: u64,
}

/// A meet connection that only holds a path through a device's self-hosted
/// relay (it says no hello), so this computer stays at that relay.
struct Keeper {
    conn: Connection,
    /// The relay's addresses it was opened for.
    relay: Vec<RelayUrl>,
    since: Instant,
    /// Its path through the relay came up.
    held: bool,
}

impl Peer {
    fn new() -> Peer {
        Peer {
            name: String::new(),
            met: None,
            lan: None,
            code_addrs: Vec::new(),
            token: None,
            invite: None,
            home: Vec::new(),
            reached_at: None,
            status: Status::Checking,
            since: None,
            conn: None,
            wake: Arc::new(Notify::new()),
            running: false,
            ask: Ask::None,
            say_no: false,
            keeper: None,
            keep_after: None,
            keep_wait: KEEP_WAIT,
            redial: 0,
        }
    }

    fn on_lan(&self) -> bool {
        self.lan.is_some_and(|(_, t)| t.elapsed() < LAN_FORGET)
    }

    fn on_lan_conn(&self) -> bool {
        self.conn.as_ref().is_some_and(is_lan_conn)
    }

    fn lan_active(&self) -> bool {
        self.on_lan() || self.on_lan_conn()
    }
}

fn is_lan_conn(c: &Connection) -> bool {
    c.close_reason().is_none() && net::direct_addr(c).is_some_and(|a| net::is_lan(a.ip()))
}

struct Issued {
    token: [u8; 8],
    text: String,
    made: Instant,
    /// Unix seconds; told to the own relays.
    expires: u64,
    /// The device that used it; only that one may use it again.
    by: Option<EndpointId>,
}

impl Issued {
    fn expired(&self) -> bool {
        self.by.is_none() && self.made.elapsed() > CODE_LIFETIME
    }
}

/// A device controlling this computer: desktop sessions, shells, commands,
/// file copies, all under one permission and one bar. It ends when the last
/// one does; after a command or copy only once nothing more came for
/// `LINGER`, so an agent's commands one after another ask once.
struct Hosting {
    id: EndpointId,
    name: String,
    _bar: ctlbar::Bar,
    uses: u32,
    idle_since: Option<Instant>,
    /// 【断开】 was clicked: it ends as soon as nothing is running.
    cut: Arc<std::sync::atomic::AtomicBool>,
}

const LINGER: Duration = Duration::from_secs(60);

struct St {
    settings: Settings,
    store: Store,
    peers: BTreeMap<EndpointId, Peer>,
    codes: Vec<Issued>,
    /// The confirmation popup is up, for this device.
    asking: Option<String>,
    hosting: Option<Hosting>,
    /// Service mode: when the lobby last asked for a snapshot.
    lobby_seen: Option<Instant>,
    lobby_window: Option<String>,
    /// A lobby window that said it closes: its last snapshots do not count.
    closed_window: Option<String>,
    /// `lobby_open` as last acted on (`lobby_check`).
    lobby_was_open: bool,
    notice: Option<(u64, String)>,
    notice_seq: u64,
    /// Made "my computer" while no service is installed: suggest installing it.
    suggest: Option<String>,
    /// Per-relay outcome of the last guest-list send: true = ok, false = fail
    /// (by the relay's first address).
    relay_ok: BTreeMap<String, bool>,
    /// The own relay standing in while the chosen one does not answer.
    using: Option<Vec<RelayUrl>>,
    /// Active hosting authorization carried across service handoff.
    handoff_host: Option<(EndpointId, String, Instant)>,
}

struct Shared {
    opts: Options,
    secret: SecretKey,
    me: EndpointId,
    /// Replaced when IPv6 comes or goes (`renew`); see `ep()`.
    ep: std::sync::RwLock<Endpoint>,
    /// What `net::ipv6_detour` said when the endpoint was made.
    detoured: std::sync::atomic::AtomicBool,
    /// Meet connections open in either direction, to say goodbye on.
    meets: Mutex<Vec<Connection>>,
    st: Mutex<St>,
    repaint: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    rt: tokio::runtime::Handle,
    lock: Mutex<Option<net::IdLock>>,
    /// `Node::stop` was called: every loop ends.
    stopped: std::sync::atomic::AtomicBool,
    /// Going away: temporary devices are off the relay's guest list.
    leaving: std::sync::atomic::AtomicBool,
    /// One guest list on its way to the relay at a time, in order.
    sending_guests: tokio::sync::Mutex<()>,
    /// Something changed: `guests_loop` looks at once.
    guests_wake: Notify,
    /// The own relays whose watch asked for the guest list (`guests_loop`
    /// sends it even when unchanged).
    resync: Mutex<BTreeSet<Vec<RelayUrl>>>,
    /// The own relays with a watch open: they say when to send the guest
    /// list; the others get it every `GUEST_SYNC_EVERY` to hear of kicks.
    watching: Mutex<BTreeSet<Vec<RelayUrl>>>,
    /// No own relay answers: the public relays are in the endpoint's map.
    fallback: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Node {
    sh: Arc<Shared>,
}

const DISCOVERY_EVERY: Duration = Duration::from_secs(2);
/// A device not seen on the LAN for this long loses its LAN card (if not listed).
const LAN_FORGET: Duration = Duration::from_secs(10);
/// How often the status of a connected device is refreshed.
const STATUS_EVERY: Duration = Duration::from_secs(2);
/// A connected device is said hello to again every this many `STATUS_EVERY`.
const HELLO_AGAIN: u32 = 5;
/// A LAN-only temporary peer is probed more often with a shorter wait so a
/// silent disconnect on the LAN is detected in ~4-6 s instead of ~10-15 s.
const LAN_HELLO_AGAIN: u32 = 2;
const LAN_ASK_WAIT: Duration = Duration::from_secs(2);
/// How often a device that is not connected is tried again (`target-plan` 2.2).
const REFIND_EVERY: Duration = Duration::from_secs(15);
const CONNECT_WAIT: Duration = Duration::from_secs(10);
/// Connected only through a public relay for this long: "cannot get through".
const UNREACHABLE_AFTER: Duration = Duration::from_secs(10);
const CODE_LIFETIME: Duration = Duration::from_secs(3600);
/// Expired codes are kept this long to tell their users so.
const CODE_KEEP: Duration = Duration::from_secs(24 * 3600);
const ASK_WAIT: Duration = Duration::from_secs(30);
/// Service mode: the lobby polls several times a second; this long without
/// a poll, its window is closed.
const LOBBY_GONE: Duration = Duration::from_secs(4);
/// No own relay connected for this long: the public relays help (if allowed).
const FALLBACK_AFTER: Duration = Duration::from_secs(10);
/// While they do, how often the own relays are asked whether they are back.
const FALLBACK_RECHECK: Duration = Duration::from_secs(30);
/// How often `keep_loop` looks at the devices.
const KEEP_EVERY: Duration = Duration::from_secs(5);
/// A new connection gets this long for its path through the relay to come up.
const KEEP_OPEN: Duration = Duration::from_secs(10);
/// After a keeper that did not get through, the next one waits this long,
/// twice as long after each one more, up to `KEEP_WAIT_MAX`.
const KEEP_WAIT: Duration = Duration::from_secs(15);
const KEEP_WAIT_MAX: Duration = Duration::from_secs(300);

fn short(id: &EndpointId) -> String {
    format!("{} {}", crate::i18n::tr("设备", "Device"), &id.to_string()[..6])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex8(s: &str) -> Option<[u8; 8]> {
    let s = s.trim();
    if s.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// The own relay `url` is an address of.
fn group_of<'a>(s: &'a Settings, url: &RelayUrl) -> Option<&'a Vec<RelayUrl>> {
    s.relays.iter().find(|g| g.contains(url))
}

/// `group` with `first` first.
fn first_of(group: &[RelayUrl], first: &RelayUrl) -> Vec<RelayUrl> {
    let mut v = vec![first.clone()];
    v.extend(group.iter().filter(|u| *u != first).cloned());
    v
}

/// The own relay the endpoint uses: the chosen one (first in the list), or
/// `using` while it stands in for the chosen one (`fallback_loop`).
fn current_group(s: &Settings, using: &Option<Vec<RelayUrl>>) -> Option<Vec<RelayUrl>> {
    using.clone().filter(|g| s.relays.contains(g)).or_else(|| s.relays.first().cloned())
}

/// Every own relay gets the guest list (`guests_loop`); the endpoint is
/// only at the one in use.
fn relays_of(s: &Settings, using: &Option<Vec<RelayUrl>>) -> net::Relays {
    net::Relays { own: current_group(s, using).unwrap_or_default(), public: s.public_relays, theirs: Vec::new(), token: None }
}

/// The relays in the endpoint's map for `r` (public fallback aside).
fn map_urls(r: &net::Relays) -> Vec<RelayUrl> {
    if !r.own.is_empty() {
        r.own.clone()
    } else if r.public {
        net::public_relays()
    } else {
        Vec::new()
    }
}

/// Up to three addresses for a connection code (`target-plan` 4.3): public
/// IPv6 first, then the IPv4 of a network with a default gateway, then
/// other IPv4 (virtual LANs); never loopback, link-local, temporary IPv6 or
/// networks that only reach virtual machines on this computer.
fn code_addrs(ep: &Endpoint) -> Vec<SocketAddr> {
    let socks = ep.bound_sockets();
    let port4 = socks.iter().find(|a| a.is_ipv4()).map(|a| a.port());
    let port6 = socks.iter().find(|a| a.is_ipv6()).map(|a| a.port());
    let (mut v6, mut gw4, mut other4) = (Vec::new(), Vec::new(), Vec::new());
    for i in netdev::get_interfaces() {
        if !i.is_up() || i.is_loopback() {
            continue;
        }
        let label = format!("{} {} {}", i.name, i.friendly_name.clone().unwrap_or_default(), i.description.clone().unwrap_or_default())
            .to_lowercase();
        if ["vethernet", "wsl", "hyper-v", "vmware", "virtualbox", "docker"].iter().any(|k| label.contains(k)) {
            continue;
        }
        if let Some(p) = port6 {
            let global = i.ipv6.iter().zip(i.ipv6_addr_flags.iter().map(Some).chain(std::iter::repeat(None))).find(|(n, f)| {
                let a = n.addr();
                (a.segments()[0] & 0xe000) == 0x2000 && f.is_none_or(|f| !f.temporary && !f.deprecated && !f.tentative)
            });
            if let Some((n, _)) = global {
                v6.push(SocketAddr::new(IpAddr::V6(n.addr()), p));
            }
        }
        if let Some(p) = port4
            && let Some(n) = i.ipv4.iter().find(|n| !n.addr().is_link_local() && !n.addr().is_loopback())
        {
            let a = SocketAddr::new(IpAddr::V4(n.addr()), p);
            if i.gateway.is_some() { gw4.push(a) } else { other4.push(a) }
        }
    }
    v6.into_iter().chain(gw4).chain(other4).take(rc_ident::conn_code::MAX_ADDRS).collect()
}

impl Shared {
    fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn changed(&self) {
        self.guests_wake.notify_one();
        if let Some(f) = self.repaint.lock().unwrap().as_ref() {
            f();
        }
    }

    /// Whether the lobby window shows. Without a lobby that reports (the
    /// `host` command), it counts as open.
    fn lobby_open(&self, st: &St) -> bool {
        (!self.opts.service && st.lobby_window.is_none()) || st.lobby_seen.is_some_and(|t| t.elapsed() < LOBBY_GONE)
    }

    /// Acts on the lobby window opening or closing (or going to the tray).
    fn lobby_check(&self, st: &mut St) -> bool {
        let open = self.lobby_open(st);
        if open == st.lobby_was_open {
            return false;
        }
        st.lobby_was_open = open;
        if open {
            log!("the lobby window is open");
        } else {
            log!("the lobby window closed: connection codes void, temporary devices forgotten");
            self.forget_strangers(st);
        }
        true
    }

    /// Codes void, temporary devices forgotten (`lobby_closed`), and their
    /// meet connections closed now, so their cards go at once over there.
    fn forget_strangers(&self, st: &mut St) {
        lobby_closed(st);
        for c in self.meets.lock().unwrap().iter() {
            if st.store.get(&c.remote_id()).is_none() {
                c.close(0u32.into(), b"forgotten");
            }
        }
    }

    /// The endpoint now. Cheap: a handle.
    fn ep(&self) -> Endpoint {
        self.ep.read().unwrap().clone()
    }

    /// Keeps `conn` to close with a reason when this computer goes away.
    fn track(&self, conn: &Connection) {
        let mut m = self.meets.lock().unwrap();
        m.retain(|c| c.close_reason().is_none());
        m.push(conn.clone());
    }

    fn notice(&self, st: &mut St, text: impl Into<String>) {
        let t = text.into();
        log!("notice: {t}");
        st.notice_seq += 1;
        st.notice = Some((st.notice_seq, t));
    }

    fn save(&self, st: &mut St) {
        if let Err(e) = st.store.save() {
            log!("{e:#}");
            let prefix = crate::i18n::tr("设备列表保存失败：", "Failed to save device list: ");
            self.notice(st, format!("{prefix}{e:#}"));
        }
    }

    /// Whether `id` should have a card and a meet connection.
    fn wanted(&self, st: &St, id: &EndpointId) -> bool {
        if self.is_stopped() {
            return false;
        }
        if st.store.devices.iter().any(|d| d.id == *id) {
            return true;
        }
        let Some(p) = st.peers.get(id) else { return false };
        self.lobby_open(st)
            && match p.met {
                Some(Met::Code) => true,
                Some(Met::Lan) => p.lan_active(),
                None => false,
            }
    }

    /// The devices this computer lets use its own relay (`target-plan` 5.2):
    /// the device list (我的电脑 or 信任设备), and the devices met with a
    /// connection code while the lobby is open and the software is not going
    /// away (temporary: forgotten when the window closes).
    fn guests(&self, st: &St) -> Vec<Guest> {
        let mut m: BTreeMap<EndpointId, Guest> = st
            .store
            .listed()
            .map(|d| (d.id, Guest::new(d.id, d.name.clone(), if d.auto_accept { GuestKind::Mine } else { GuestKind::Trusted })))
            .collect();
        if self.lobby_open(st) && !self.is_stopped() && !self.leaving.load(std::sync::atomic::Ordering::SeqCst) {
            for (id, p) in &st.peers {
                if p.met == Some(Met::Code) {
                    m.entry(*id).or_insert_with(|| Guest::new(*id, p.name.clone(), GuestKind::Temp));
                }
            }
        }
        m.into_values().collect()
    }

    /// Settings changed in `st` (the relays were `before`): saved, every
    /// device hears of it, and the endpoint uses the relays now in them.
    /// Another relay in use voids the connection codes not used yet: they
    /// point at where this computer no longer is.
    fn settings_changed(&self, st: &mut St, before: net::Relays) {
        if let Err(e) = st.settings.save(&store::settings_path(&self.opts.key)) {
            let prefix = crate::i18n::tr("设置保存失败：", "Failed to save settings: ");
            let msg = format!("{prefix}{e:#}");
            self.notice(st, msg);
        }
        for p in st.peers.values() {
            p.wake.notify_one();
        }
        let after = relays_of(&st.settings, &st.using);
        if map_urls(&before) != map_urls(&after) {
            let n = st.codes.iter().filter(|c| c.by.is_none()).count();
            st.codes.retain(|c| c.by.is_some());
            if n > 0 {
                log!("the relay in use changed: {n} connection code(s) not used yet are void");
            }
        }
        if before != after {
            let ep = self.ep();
            tokio::spawn(async move { apply_relays(&ep, &before, &after).await });
        }
    }

    /// The connection codes not used yet, for the own relays (none while
    /// the lobby is closed or the software is going away).
    fn invites(&self, st: &St) -> Vec<Invite> {
        if !self.lobby_open(st) || self.is_stopped() || self.leaving.load(std::sync::atomic::Ordering::SeqCst) {
            return Vec::new();
        }
        st.codes.iter().filter(|c| c.by.is_none() && !c.expired()).map(|c| Invite { token: c.token, expires: c.expires }).collect()
    }

    /// Replaces this computer's invites on `relay` (its addresses).
    async fn send_invites(&self, relay: &[RelayUrl], invites: &[Invite]) -> Result<()> {
        let _one = self.sending_guests.lock().await;
        rc_relay::client::set_invites(relay, &self.secret, &rc_relay::tls::ca(), invites).await.map_err(relay_error)?;
        log!("connection codes on {}: {} not used yet", relay[0], invites.len());
        Ok(())
    }

    /// Removes `id` from the device list or active temporary peers and tells
    /// it to forget this computer too.
    fn drop_guest(self: &Arc<Self>, id: EndpointId, why: &str) -> bool {
        let mut st = self.st.lock().unwrap();
        let deleted = if st.store.forget(&id) {
            log!("{why}: deleted {} from the device list", id.fmt_short());
            true
        } else if let Some(name) = st.peers.get(&id).filter(|p| p.met == Some(Met::Code)).map(|p| p.name.clone()) {
            // Kept as deleted until it knows, like a device of the list.
            st.store.add(id).name = name;
            st.store.forget(&id);
            log!("{why}: deleted {} (met with a code)", id.fmt_short());
            true
        } else {
            false
        };
        if deleted {
            self.save(&mut st);
            if let Some(p) = st.peers.get_mut(&id) {
                p.met = None;
                p.token = None;
                p.invite = None;
                p.ask = Ask::None;
                p.say_no = false;
                // A connection it opened: closed now, so it calls again, is
                // refused and drops its card. Over this computer's own
                // connection the woken task tells it instead.
                if let Some(c) = p.conn.take_if(|c| c.side().is_server()) {
                    c.close(0u32.into(), b"forgotten");
                    p.status = Status::Offline;
                    p.since = None;
                }
            }
            self.ensure(&mut st, id);
        }
        deleted
    }

    /// Replaces this computer's guest list on `relay` (its addresses),
    /// returning any guest IDs the relay kicked and asked this computer to drop.
    /// The error is the relay's own (a `Refusal`, see `relay_error`).
    async fn send_guests(&self, relay: &[RelayUrl], guests: &[Guest]) -> Result<Vec<EndpointId>> {
        let _one = self.sending_guests.lock().await;
        let dropped = rc_relay::client::set_guests(relay, &self.secret, &rc_relay::tls::ca(), guests).await?;
        log!("guest list on {}: {} device(s)", relay[0], guests.len());
        Ok(dropped)
    }

    /// Pushes the current connection codes to every own relay in parallel on
    /// the background runtime, without blocking `issue_code` or letting one
    /// unreachable relay stall the others. `guests_loop` still retries if any
    /// push fails.
    fn send_invites_now(self: &Arc<Self>) {
        let (groups, list) = {
            let st = self.st.lock().unwrap();
            (st.settings.relays.clone(), self.invites(&st))
        };
        for g in groups {
            let sh = self.clone();
            let list = list.clone();
            self.rt.spawn(async move {
                let r = tokio::time::timeout(Duration::from_secs(3), sh.send_invites(&g, &list)).await;
                if let Err(e) = r.unwrap_or_else(|_| Err(anyhow!("no answer within 3 s"))) {
                    log!("connection codes on {}: {e:#}", g[0]);
                }
            });
        }
    }

    /// Brings the token of `owner`'s connection code to the owner's relay,
    /// so the relay lets the two reach each other (see the module comment).
    /// Done once the relay answers; tried again with the next dial if not.
    async fn bring_code(&self, owner: EndpointId, relay: &[RelayUrl], token: [u8; 8], round: u32) {
        let ca = rc_relay::tls::ca();
        let name = self.st.lock().unwrap().settings.shown_name();
        let meet = rc_relay::client::meet(relay, &self.secret, &name, &ca, owner, token);
        let r = tokio::time::timeout(Duration::from_secs(5), meet).await.unwrap_or_else(|_| Err(anyhow!("no answer within 5 s")));
        let done = match r {
            Ok(()) => {
                log!("brought the connection code of {} to its relay {}", owner.fmt_short(), relay[0]);
                true
            }
            Err(e) if e.downcast_ref::<Refusal>().is_some() => {
                log!("the connection code of {} at its relay {}: {e:#}", owner.fmt_short(), relay[0]);
                true
            }
            Err(e) => {
                if round <= 1 {
                    log!("the connection code of {} at its relay {}: {e:#}", owner.fmt_short(), relay[0]);
                }
                false
            }
        };
        if done && let Some(p) = self.st.lock().unwrap().peers.get_mut(&owner) {
            p.invite = None;
        }
    }

    /// Where this computer is reachable now: its home relay, with the other
    /// addresses of the same own relay.
    fn home_list(&self, st: &St) -> Vec<RelayUrl> {
        match net::home_relay(&self.ep()) {
            Some(h) => group_of(&st.settings, &h).map_or_else(|| vec![h.clone()], |g| first_of(g, &h)),
            None => Vec::new(),
        }
    }

    /// Makes sure `id` has a peer entry and a running task.
    fn ensure(self: &Arc<Self>, st: &mut St, id: EndpointId) {
        if id == self.me {
            return;
        }
        let p = st.peers.entry(id).or_insert_with(Peer::new);
        if !p.running {
            p.running = true;
            let wake = p.wake.clone();
            tokio::spawn(peer_loop(self.clone(), id, wake));
        } else {
            p.wake.notify_one();
        }
    }

    fn shown_name(&self, st: &St, id: &EndpointId) -> String {
        if let Some(d) = st.store.get(id)
            && !d.name.is_empty()
        {
            return d.name.clone();
        }
        match st.peers.get(id) {
            Some(p) if !p.name.is_empty() => p.name.clone(),
            _ => short(id),
        }
    }

    /// Where to look for `id`: its addresses, and relays (its home, this
    /// computer's own relay, and the public ones every other round when its
    /// home is unknown or did not work).
    fn dial_plan(&self, st: &St, id: &EndpointId, round: u32) -> (Vec<SocketAddr>, Vec<RelayUrl>) {
        let p = st.peers.get(id);
        let d = st.store.devices.iter().find(|d| d.id == *id);
        let mut addrs: Vec<SocketAddr> = Vec::new();
        if let Some(p) = p {
            if let Some((a, _)) = p.lan.filter(|_| p.on_lan()) {
                addrs.push(a);
            }
            addrs.extend(p.reached_at);
            addrs.extend(p.code_addrs.iter().copied());
        }
        if let Some(d) = d {
            addrs.extend(d.addrs.iter().copied());
        }
        let mut seen = BTreeSet::new();
        addrs.retain(|a| seen.insert(*a));
        // A LAN-only temporary peer stays on the LAN; never dial it via relays.
        if d.is_none() && p.is_some_and(|p| p.met == Some(Met::Lan)) {
            addrs.retain(|a| net::is_lan(a.ip()));
            return (addrs, Vec::new());
        }
        let public_ok = st.settings.public_relays;
        let mut relays: Vec<RelayUrl> = Vec::new();
        let home: Vec<RelayUrl> = match p.map(|p| &p.home).filter(|h| !h.is_empty()) {
            Some(h) => h.clone(),
            None => d.map(|d| d.relays.clone()).unwrap_or_default(),
        };
        relays.extend(home.iter().filter(|h| public_ok || net::public_index(h).is_none()).cloned());
        relays.extend(current_group(&st.settings, &st.using).unwrap_or_default());
        if public_ok && (home.is_empty() || round % 2 == 1) {
            relays.extend(net::public_relays());
        }
        let mut seen = BTreeSet::new();
        relays.retain(|r| seen.insert(r.to_string()));
        (addrs, relays)
    }

    /// Where `id` stands in this computer's device list, for its hello or
    /// welcome (`meet::EVENT_*`).
    fn wish(&self, st: &St, id: &EndpointId) -> u8 {
        let p = st.peers.get(id);
        if st.store.is_forgotten(id) {
            meet::EVENT_FORGET
        } else if st.store.get(id).is_some_and(|d| d.tell) {
            meet::EVENT_REMEMBER
        } else if st.store.get(id).is_some() {
            meet::EVENT_NONE
        } else if p.is_some_and(|p| p.say_no) {
            meet::EVENT_NO
        } else if p.is_some_and(|p| p.ask == Ask::Out) {
            meet::EVENT_ASK
        } else {
            meet::EVENT_NONE
        }
    }

    /// `id` and this computer remember each other now: in the device list as
    /// a trusted device. `tell`: it does not list this computer yet.
    fn befriend(self: &Arc<Self>, st: &mut St, id: EndpointId, tell: bool) {
        let name = st.peers.get(&id).map(|p| p.name.clone()).unwrap_or_default();
        let d = st.store.add(id);
        if d.name.is_empty() {
            d.name = name;
        }
        d.tell = tell;
        if let Some(p) = st.peers.get_mut(&id) {
            p.ask = Ask::None;
            p.say_no = false;
        }
        self.save(st);
        log!("{} and this computer remember each other now (trusted)", id.fmt_short());
        self.ensure(st, id);
    }

    /// What `from` said of this computer's place in its list, in a hello or
    /// a welcome: remembering takes both sides (`devices-and-relay.md` 3.3).
    fn heard(self: &Arc<Self>, st: &mut St, from: EndpointId, event: u8) {
        let listed = st.store.get(&from).is_some();
        let ask = st.peers.get(&from).map_or(Ask::None, |p| p.ask);
        let refusing = st.peers.get(&from).is_some_and(|p| p.say_no);
        let name = self.shown_name(st, &from);
        match event {
            // Asked again before it heard the refusal (which goes back now).
            meet::EVENT_ASK | meet::EVENT_REMEMBER if !listed && refusing => {}
            // It asks though this computer lists it: it lost this one; it is told again.
            meet::EVENT_ASK if listed => {
                if let Some(d) = st.store.get_mut(&from)
                    && !d.tell
                {
                    d.tell = true;
                    self.save(st);
                    self.ensure(st, from);
                }
            }
            meet::EVENT_ASK | meet::EVENT_REMEMBER if !listed && ask == Ask::Out => {
                self.befriend(st, from, event == meet::EVENT_ASK);
                let text = if crate::i18n::is_en() {
                    if event == meet::EVENT_ASK {
                        format!("You and \"{name}\" remembered each other")
                    } else {
                        format!("\"{name}\" agreed; you remembered each other")
                    }
                } else if event == meet::EVENT_ASK {
                    format!("已和「{name}」互相记住")
                } else {
                    format!("「{name}」同意了，已互相记住")
                };
                self.notice(st, text);
            }
            // Listing this computer without being asked (an older version, or
            // a request that lapsed here) counts as a request.
            meet::EVENT_ASK | meet::EVENT_REMEMBER if !listed && ask == Ask::None => {
                if let Some(p) = st.peers.get_mut(&from) {
                    p.ask = Ask::In;
                    log!("{} ({name}) asks to remember each other", from.fmt_short());
                }
            }
            meet::EVENT_NO => {
                if let Some(p) = st.peers.get_mut(&from) {
                    p.ask = Ask::None;
                }
                if ask == Ask::Out || listed {
                    log!("{} ({name}) did not agree to remember each other", from.fmt_short());
                    if listed && st.store.remove(&from) {
                        self.save(st);
                    }
                    let msg = if crate::i18n::is_en() {
                        format!("\"{name}\" declined")
                    } else {
                        format!("「{name}」没有同意")
                    };
                    self.notice(st, msg);
                }
            }
            _ => {}
        }
    }

    fn hello(&self, st: &St, id: &EndpointId) -> (Msg, u8) {
        let event = self.wish(st, id);
        let home = url_list(&self.home_list(st));
        let token = st.peers.get(id).and_then(|p| p.token);
        (Msg::MeetHi { name: st.settings.shown_name(), home, token, event, grant: Some(String::new()) }, event)
    }

    /// Status of a connected device from its connection's selected path.
    fn path_status(&self, st: &mut St, id: &EndpointId) {
        let Some(p) = st.peers.get_mut(id) else { return };
        let Some(conn) = &p.conn else { return };
        let (kind, _, rtt) = net::selected_path(conn);
        let stuck = kind == PathKind::Public
            && !net::has_direct_path(conn)
            && p.since.is_some_and(|t| t.elapsed() > UNREACHABLE_AFTER);
        p.status = if stuck { Status::Unreachable } else { Status::Online { rtt_ms: rtt, path: kind } };
        let direct = net::direct_addr(conn);
        if direct.is_some() {
            p.reached_at = direct;
        }
        // A listed device: remember where it was reached directly.
        if let Some(a) = direct
            && let Some(d) = st.store.get_mut(id)
            && d.addrs.first() != Some(&a)
        {
            d.addrs.retain(|x| *x != a);
            d.addrs.insert(0, a);
            d.addrs.truncate(4);
            self.save(st);
        }
    }

    /// `event`: where this computer stands in its list (see `heard`).
    #[allow(clippy::too_many_arguments)]
    fn welcomed(self: &Arc<Self>, st: &mut St, id: &EndpointId, conn: &Connection, name: String, home: String, sent: u8, event: u8) {
        if sent == meet::EVENT_FORGET {
            // Re-added (e.g. a freshly pasted connection code) while this delete
            // was in flight: keep the new peer state intact.
            if !st.store.is_forgotten(id) {
                return;
            }
            log!("{} knows it was deleted here", id.fmt_short());
            st.store.remove(id);
            self.save(st);
            if let Some(p) = st.peers.get_mut(id) {
                p.lan = None;
                p.met = None;
                p.conn = None;
                p.since = None;
                p.status = Status::Offline;
            }
            return;
        }
        let name = clean(&name);
        let home = relay_urls(&home);
        if let Some(p) = st.peers.get_mut(id) {
            // Met: the code has done its work. Shown again after the other side
            // forgot this computer, it would only read as a void code.
            p.token = None;
            p.invite = None;
            if !name.is_empty() {
                p.name = name.clone();
            }
            if !home.is_empty() {
                p.home = home.clone();
            }
            if p.conn.as_ref().is_none_or(|c| c.stable_id() != conn.stable_id()) {
                p.conn = Some(conn.clone());
                p.since = Some(Instant::now());
            }
        }
        match sent {
            meet::EVENT_REMEMBER => {
                if let Some(d) = st.store.get_mut(id) {
                    d.tell = false;
                }
                self.save(st);
            }
            meet::EVENT_NO => {
                if let Some(p) = st.peers.get_mut(id) {
                    p.say_no = false;
                }
            }
            _ => {}
        }
        self.heard(st, *id, event);
        if let Some(d) = st.store.get_mut(id) {
            let mut dirty = false;
            if d.name.is_empty() && !name.is_empty() {
                d.name = name;
                dirty = true;
            }
            if !home.is_empty() && d.relays != home {
                d.relays = home;
                dirty = true;
            }
            if dirty {
                self.save(st);
            }
        }
        self.path_status(st, id);
    }

    fn refused(&self, st: &mut St, id: &EndpointId, reason: u8, sent: u8) {
        log!("{} refused: {}", id.fmt_short(), meet::refusal_text(reason));
        let name = self.shown_name(st, id);
        if sent == meet::EVENT_FORGET {
            st.store.remove(id);
            self.save(st);
        }
        match reason {
            meet::REFUSE_FORGOTTEN => {
                if st.store.remove(id) {
                    self.save(st);
                    let msg = if crate::i18n::is_en() {
                        format!("\"{name}\" removed this computer")
                    } else {
                        format!("「{name}」把这台电脑删除了")
                    };
                    self.notice(st, msg);
                }
                if let Some(p) = st.peers.get_mut(id) {
                    p.met = None;
                }
            }
            meet::REFUSE_CODE_EXPIRED | meet::REFUSE_CODE_USED | meet::REFUSE_CODE_UNKNOWN => {
                let prefix = crate::i18n::tr("连接失败：", "Connection failed: ");
                self.notice(st, format!("{prefix}{}", meet::refusal_text(reason)));
                if let Some(p) = st.peers.get_mut(id) {
                    p.token = None;
                    if p.met == Some(Met::Code) {
                        p.met = None;
                    }
                }
            }
            _ => {
                if st.store.get(id).is_none()
                    && let Some(p) = st.peers.get_mut(id)
                {
                    // A LAN device that does not take strangers now (its window is closed).
                    p.met = None;
                }
            }
        }
        if let Some(p) = st.peers.get_mut(id) {
            p.status = Status::Refused(reason);
            p.conn = None;
            p.since = None;
        }
    }

    /// Whether a hello from `id` is welcome without a code: it is in the
    /// device list, or it was met while the lobby is open.
    fn knows(&self, st: &St, id: &EndpointId) -> bool {
        st.store.get(id).is_some()
            || (self.lobby_open(st)
                && st.peers.get(id).is_some_and(|p| match p.met {
                    Some(Met::Code) => true,
                    Some(Met::Lan) => p.lan_active(),
                    None => false,
                }))
    }

    /// The answer to a hello from `from` (see `meet`).
    fn on_hi(self: &Arc<Self>, from: EndpointId, conn: &Connection, name: String, home: String, token: Option<[u8; 8]>, event: u8) -> Msg {
        let mut st = self.st.lock().unwrap();
        let welcome = |st: &St, event: u8| Msg::MeetWelcome {
            name: st.settings.shown_name(),
            home: url_list(&self.home_list(st)),
            grant: Some(String::new()),
            event,
        };
        let name = clean(&name);
        if event == meet::EVENT_FORGET && token.is_none() {
            log!("{} deleted this computer: deleting it too", from.fmt_short());
            let shown = self.shown_name(&st, &from);
            let mut had = st.store.get(&from).is_some();
            if st.store.devices.iter().any(|d| d.id == from) {
                st.store.remove(&from);
                self.save(&mut st);
            }
            if let Some(p) = st.peers.get_mut(&from) {
                had |= p.met.is_some();
                p.lan = None;
                p.met = None;
                p.ask = Ask::None;
                p.say_no = false;
                // Its own connection ends now, not at its next status round.
                p.wake.notify_one();
            }
            if had {
                let msg = if crate::i18n::is_en() {
                    format!("\"{shown}\" removed this computer")
                } else {
                    format!("「{shown}」把这台电脑删除了")
                };
                self.notice(&mut st, msg);
            }
            let w = welcome(&st, meet::EVENT_NONE);
            drop(st);
            self.changed();
            return w;
        }
        let open = self.lobby_open(&st);
        let known = |st: &St| self.knows(st, &from);
        if let Some(t) = token
            && !known(&st)
        {
            let refuse = match st.codes.iter().find(|c| c.token == t) {
                _ if !open => Some(meet::REFUSE_CODE_UNKNOWN),
                None => Some(meet::REFUSE_CODE_UNKNOWN),
                Some(c) if c.by.is_some_and(|b| b != from) => Some(meet::REFUSE_CODE_USED),
                Some(c) if c.expired() => Some(meet::REFUSE_CODE_EXPIRED),
                Some(_) => None,
            };
            if let Some(r) = refuse {
                log!("{} ({name}) brought a code: refused ({})", from.fmt_short(), meet::refusal_text(r));
                return Msg::MeetRefuse { reason: r };
            }
            if let Some(c) = st.codes.iter_mut().find(|c| c.token == t) {
                c.by = Some(from);
            }
            log!("{} ({name}) used a connection code", from.fmt_short());
            if st.store.is_forgotten(&from) {
                st.store.remove(&from);
                self.save(&mut st);
            }
            st.peers.entry(from).or_insert_with(Peer::new).met = Some(Met::Code);
        }
        if st.store.is_forgotten(&from) {
            return Msg::MeetRefuse { reason: meet::REFUSE_FORGOTTEN };
        }
        if !known(&st) {
            let lan_direct = net::direct_addr(conn).filter(|a| net::is_lan(a.ip()));
            let lan = st.peers.get(&from).is_some_and(|p| p.on_lan()) || lan_direct.is_some();
            if !(open && lan && st.settings.lan_visible) {
                return Msg::MeetRefuse { reason: meet::REFUSE_UNKNOWN };
            }
            let p = st.peers.entry(from).or_insert_with(Peer::new);
            p.met = Some(Met::Lan);
            if let Some(a) = lan_direct {
                p.reached_at = Some(a);
            }
        }
        {
            let p = st.peers.entry(from).or_insert_with(Peer::new);
            if !name.is_empty() {
                p.name = name;
            }
            let h = relay_urls(&home);
            if !h.is_empty() {
                p.home = h;
            }
        }
        self.heard(&mut st, from, event);
        // Waking a connected peer task here would make the two sides' hellos wake
        // each other forever; only an idle one (not connected) needs it.
        if st.peers.get(&from).is_none_or(|p| !p.running || p.conn.is_none()) {
            self.ensure(&mut st, from);
        }
        let mine = self.wish(&st, &from);
        if mine == meet::EVENT_NO
            && let Some(p) = st.peers.get_mut(&from)
        {
            p.say_no = false;
        }
        let w = welcome(&st, mine);
        drop(st);
        self.changed();
        w
    }
}

const HANDOFF_TTL_SECS: u64 = 90;

struct HandoffData {
    hosting: Option<(EndpointId, String, u64)>,
    peers: Vec<(EndpointId, Peer)>,
    codes: Vec<Issued>,
}

fn handoff_path(key: &Path) -> PathBuf {
    key.with_file_name("handoff.txt")
}

fn save_handoff(path: &Path, st: &St) -> Result<()> {
    save_handoff_at(path, st, rc_ident::proof::unix_now())
}

fn save_handoff_at(path: &Path, st: &St, now_unix: u64) -> Result<()> {
    let hosting = st
        .hosting
        .as_ref()
        .filter(|h| !h.cut.load(std::sync::atomic::Ordering::SeqCst))
        .map(|h| (h.id, h.name.as_str()));
    write_handoff(path, hosting, &st.peers, &st.store, &st.codes, now_unix)
}

fn write_handoff(
    path: &Path,
    hosting: Option<(EndpointId, &str)>,
    peers: &BTreeMap<EndpointId, Peer>,
    store: &Store,
    codes: &[Issued],
    now_unix: u64,
) -> Result<()> {
    let expires_unix = now_unix.saturating_add(HANDOFF_TTL_SECS);
    let mut text = format!("# remote-control handoff v1\nexpires\t{expires_unix}\n");
    if let Some((id, name)) = hosting {
        text += &format!("hosting\t{id}\t{}\t{expires_unix}\n", clean(name));
    }
    for (id, p) in peers {
        if p.met != Some(Met::Code) || store.get(id).is_some() || store.is_forgotten(id) {
            continue;
        }
        let mut addrs: Vec<SocketAddr> = Vec::new();
        if let Some(a) = p.reached_at {
            addrs.push(a);
        }
        for &a in &p.code_addrs {
            if !addrs.contains(&a) {
                addrs.push(a);
            }
        }
        let addrs_str: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        let token = p.token.map(|t| hex(&t)).unwrap_or_default();
        let invite = p.invite.as_deref().map(url_list).unwrap_or_default();
        text += &format!(
            "peer\t{id}\t{}\t{}\t{}\t{token}\t{invite}\n",
            clean(&p.name),
            url_list(&p.home),
            addrs_str.join(",")
        );
    }
    for c in codes {
        if c.expired() {
            continue;
        }
        let code_text = c.text.replace(['\t', '\r', '\n'], "");
        let by = c.by.map(|id| id.to_string()).unwrap_or_default();
        text += &format!("code\t{}\t{code_text}\t{}\t{}\t{by}\n", hex(&c.token), c.expires, c.made.elapsed().as_secs());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

fn load_handoff(path: &Path) -> Option<HandoffData> {
    load_handoff_at(path, rc_ident::proof::unix_now())
}

fn load_handoff_at(path: &Path, now_unix: u64) -> Option<HandoffData> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    parse_handoff(&text, now_unix)
}

fn parse_handoff(text: &str, now_unix: u64) -> Option<HandoffData> {
    let mut expires: Option<u64> = None;
    let mut hosting: Option<(EndpointId, String, u64)> = None;
    let mut peers: Vec<(EndpointId, Peer)> = Vec::new();
    let mut codes: Vec<Issued> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "expires" => {
                let exp: u64 = f.get(1)?.trim().parse().ok()?;
                expires = Some(exp);
            }
            "hosting" => {
                if f.len() < 4 {
                    return None;
                }
                let id: EndpointId = f[1].trim().parse().ok()?;
                let name = clean(f[2]);
                let h_exp: u64 = f[3].trim().parse().ok()?;
                if now_unix < h_exp {
                    let rem = (h_exp - now_unix).min(HANDOFF_TTL_SECS);
                    hosting = Some((id, name, rem));
                }
            }
            "peer" => {
                if f.len() < 7 {
                    return None;
                }
                let id: EndpointId = f[1].trim().parse().ok()?;
                let mut p = Peer::new();
                p.name = clean(f[2]);
                p.met = Some(Met::Code);
                p.home = relay_urls(f[3]);
                p.code_addrs = f[4]
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .filter_map(|s| s.trim().parse::<SocketAddr>().ok())
                    .collect();
                p.token = if f[5].trim().is_empty() { None } else { Some(unhex8(f[5])?) };
                p.invite = if f[6].trim().is_empty() {
                    None
                } else {
                    let inv = relay_urls(f[6]);
                    (!inv.is_empty()).then_some(inv)
                };
                peers.push((id, p));
            }
            "code" => {
                if f.len() < 6 {
                    return None;
                }
                let token = unhex8(f[1])?;
                let code_text = f[2].trim().to_string();
                if code_text.is_empty() {
                    return None;
                }
                let code_expires: u64 = f[3].trim().parse().ok()?;
                let age_s: u64 = f[4].trim().parse().ok()?;
                let by = if f[5].trim().is_empty() {
                    None
                } else {
                    Some(f[5].trim().parse::<EndpointId>().ok()?)
                };
                let made = Instant::now().checked_sub(Duration::from_secs(age_s)).unwrap_or_else(Instant::now);
                let issued = Issued { token, text: code_text, made, expires: code_expires, by };
                if !issued.expired() {
                    codes.push(issued);
                }
            }
            _ => return None,
        }
    }
    let exp = expires?;
    if now_unix >= exp {
        return None;
    }
    Some(HandoffData { hosting, peers, codes })
}

impl Node {
    /// Goes online with the device key at `opts.key`.
    pub async fn start(opts: Options) -> Result<Node> {
        let secret = net::load_key(&opts.key)?;
        let wait = if opts.service { Duration::from_secs(10) } else { Duration::ZERO };
        let key = opts.key.clone();
        let lock = tokio::task::spawn_blocking(move || net::IdLock::acquire(&key, wait)).await??;
        let settings = Settings::load(&store::settings_path(&opts.key));
        crate::i18n::set_pref(settings.language);
        let mut store = Store::load(&store::devices_path(&opts.key)).unwrap_or_else(|e| {
            log!("{e:#}; starting with an empty device list");
            Store::empty(&store::devices_path(&opts.key))
        });
        let mut dirty = false;
        // Phase 1 kept test viewers in allowed.txt.
        let allowed = opts.key.with_file_name("allowed.txt");
        let mut allow = opts.allow.clone();
        if let Ok(text) = std::fs::read_to_string(&allowed) {
            allow.extend(text.lines().filter_map(|l| l.trim().parse::<EndpointId>().ok()));
            let _ = std::fs::remove_file(&allowed);
            dirty = true;
        }
        for id in allow {
            let d = store.add(id);
            if !d.auto_accept {
                d.auto_accept = true;
                dirty = true;
            }
        }
        for f in ["lobby-id.txt", "service-id.txt"] {
            let _ = std::fs::remove_file(opts.key.with_file_name(f));
        }
        if dirty {
            store.save()?;
        }
        let handoff = load_handoff(&handoff_path(&opts.key));
        let detoured = net::ipv6_detour().is_some();
        let ep = net::endpoint(secret.clone(), &relays_of(&settings, &None), true).await?;
        let me = ep.id();
        log!("node: device id {me}{}", if opts.service { " (service)" } else { "" });
        for a in ep.bound_sockets() {
            log!("node: listening on {a}");
        }
        crate::power::STAY_AWAKE.store(settings.stay_awake, std::sync::atomic::Ordering::SeqCst);
        if opts.service
            && let Err(e) = crate::install::set_auto_start(settings.boot_online)
        {
            log!("set auto start: {e:#}");
        }
        let mut peers = BTreeMap::new();
        let mut codes = Vec::new();
        let mut handoff_host = None;
        let (mut lobby_seen, mut lobby_was_open) = (None, !opts.service);
        if let Some(h) = handoff {
            log!(
                "node: loaded handoff state ({} peer(s), {} code(s), hosting: {})",
                h.peers.len(),
                h.codes.len(),
                h.hosting.is_some()
            );
            for (id, p) in h.peers {
                if store.get(&id).is_none() && !store.is_forgotten(&id) {
                    peers.insert(id, p);
                }
            }
            codes = h.codes;
            handoff_host = h.hosting.map(|(id, name, rem)| (id, name, Instant::now() + Duration::from_secs(rem)));
            lobby_seen = Some(Instant::now());
            lobby_was_open = true;
        }
        let st = St {
            settings,
            store,
            peers,
            codes,
            asking: None,
            hosting: None,
            lobby_seen,
            lobby_window: None,
            closed_window: None,
            lobby_was_open,
            notice: None,
            notice_seq: 0,
            suggest: None,
            relay_ok: BTreeMap::new(),
            using: None,
            handoff_host,
        };
        let sh = Arc::new(Shared { opts, secret, me, ep: std::sync::RwLock::new(ep.clone()), st: Mutex::new(st), repaint: Mutex::default(), rt: tokio::runtime::Handle::current(),
            detoured: detoured.into(),
            meets: Mutex::default(),
            lock: Mutex::new(Some(lock)),
            stopped: Default::default(),
            leaving: Default::default(),
            sending_guests: Default::default(),
            guests_wake: Notify::new(),
            resync: Mutex::default(),
            watching: Mutex::default(),
            fallback: Default::default(),
        });
        {
            let mut st = sh.st.lock().unwrap();
            let ids: Vec<EndpointId> = st.store.devices.iter().map(|d| d.id).chain(st.peers.keys().copied()).collect();
            for id in ids {
                sh.ensure(&mut st, id);
            }
        }
        serve_endpoint(&sh, &ep);
        if sh.opts.lan {
            tokio::spawn(discovery_loop(sh.clone()));
        }
        tokio::spawn(housekeeping(sh.clone()));
        tokio::spawn(guests_loop(sh.clone()));
        tokio::spawn(fallback_loop(sh.clone()));
        tokio::spawn(keep_loop(sh.clone()));
        tokio::spawn(v6_loop(sh.clone()));
        Ok(Node { sh })
    }

    /// Goes offline for good (the service takes over): sessions end, the
    /// endpoint closes, the device key is free for another process.
    pub async fn stop(&self) {
        self.sh.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        for p in self.sh.st.lock().unwrap().peers.values() {
            p.wake.notify_one();
        }
        self.goodbye(net::CLOSE_STOPPED).await;
        self.sh.st.lock().unwrap().hosting = None;
        self.sh.lock.lock().unwrap().take();
        log!("node stopped");
    }

    /// Goes offline temporarily to hand off to the background service: saves
    /// transient state to `handoff.txt`, closes peers/sessions with
    /// `CLOSE_HANDOFF`, keeps own-relay guests/invites, and frees `IdLock`.
    pub async fn stop_for_handoff(&self) {
        {
            let st = self.sh.st.lock().unwrap();
            if let Err(e) = save_handoff(&handoff_path(&self.sh.opts.key), &st) {
                log!("handoff save failed: {e:#}");
            }
        }
        for c in std::mem::take(&mut *self.sh.meets.lock().unwrap()) {
            c.close(net::CLOSE_HANDOFF.into(), b"handoff");
        }
        self.sh.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        for p in self.sh.st.lock().unwrap().peers.values() {
            p.wake.notify_one();
        }
        let n = host::session_count();
        log!("node handing off (close code {}): closing {n} session(s)", net::CLOSE_HANDOFF);
        host::close_sessions(net::CLOSE_HANDOFF);
        let _ = tokio::time::timeout(Duration::from_millis(1500), self.sh.ep().close()).await;
        self.sh.st.lock().unwrap().hosting = None;
        self.sh.lock.lock().unwrap().take();
        log!("node stopped for handoff");
    }

    /// Ends hosted sessions with `code` and closes the endpoint. The own
    /// relay's guest list loses the temporary devices first, and the
    /// connection codes not used yet go.
    pub async fn goodbye(&self, code: u32) {
        self.sh.leaving.store(true, std::sync::atomic::Ordering::SeqCst);
        // First, so the cards on the other side go at once (`departed`).
        for c in std::mem::take(&mut *self.sh.meets.lock().unwrap()) {
            c.close(net::CLOSE_STOPPED.into(), b"leaving");
        }
        let (relays, list, codes) = {
            let st = self.sh.st.lock().unwrap();
            (st.settings.relays.clone(), self.sh.guests(&st), st.codes.iter().any(|c| c.by.is_none() && !c.expired()))
        };
        let mut handles = Vec::new();
        for relay in relays {
            let sh = self.sh.clone();
            let list = list.clone();
            handles.push(tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), sh.send_guests(&relay, &list)).await;
                if codes {
                    let _ = tokio::time::timeout(Duration::from_secs(2), sh.send_invites(&relay, &[])).await;
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        let n = host::session_count();
        log!("node going away (close code {code}): closing {n} session(s)");
        host::close_sessions(code);
        let _ = tokio::time::timeout(Duration::from_millis(1500), self.sh.ep().close()).await;
    }

    /// One request from the lobby (see the module comment and `lobby`).
    pub fn request(&self, line: &str) -> String {
        let _rt = self.sh.rt.enter();
        let f: Vec<&str> = line.split('\t').collect();
        let id = |i: usize| f.get(i).and_then(|s| s.trim().parse::<EndpointId>().ok());
        let r = match f[0] {
            "snapshot" => Ok(self.snapshot(true, f.get(1).copied())),
            "peek" => Ok(self.snapshot(false, None)),
            "lobby-closed" => Ok(self.lobby_closing(f.get(1).copied().unwrap_or(""))),
            "log" => self.log_tail(f.get(1).and_then(|n| n.parse().ok()).unwrap_or(200)),
            "code" => self.issue_code(),
            "revoke" => self.revoke(f.get(1).copied().unwrap_or("")),
            "paste" => self.paste(f.get(1).copied().unwrap_or("")),
            "rename" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.rename(i, f.get(2).copied().unwrap_or(""))),
            "remove" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.remove(i)),
            "auto" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.set_auto(i, f.get(2) == Some(&"1"))),
            "remember" => id(1).ok_or_else(|| anyhow!("bad id")).and_then(|i| self.remember(i)),
            "agree" => id(1).ok_or_else(|| anyhow!("bad id")).and_then(|i| self.agree(i)),
            "refuse" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.refuse(i)),
            "cancel-remember" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.cancel_remember(i)),
            "retry" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.retry(i)),
            "proof" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| ident::issue(&self.sh.secret, i)),
            "dial" => id(1).ok_or_else(|| anyhow!("bad id")).map(|i| self.dial(i)),
            "set" => {
                let key = f.get(1).copied().unwrap_or("");
                let on = f.get(2) == Some(&"1");
                match key {
                    "public" => Ok(self.settings(|s| s.public_relays = on)),
                    "lan" => Ok(self.settings(|s| s.lan_visible = on)),
                    "boot" => Ok(self.settings(|s| s.boot_online = on)),
                    "lock" => Ok(self.settings(|s| s.lock_control = on)),
                    "awake" => Ok(self.settings(|s| s.stay_awake = on)),
                    _ => Err(anyhow!("unknown settings key {key}")),
                }
            }
            "set-name" => Ok(self.settings(|s| s.name = clean(f.get(1).copied().unwrap_or("")))),
            "set-language" => Ok(self.settings(|s| s.language = crate::i18n::LangPref::parse(f.get(1).copied().unwrap_or("")))),
            "set-public" => Ok(self.settings(|s| s.public_relays = f.get(1) == Some(&"1"))),
            "relay-claim" => self.claim_relay(f.get(1).copied().unwrap_or("")),
            "relay-use" => self.use_relay(f.get(1).copied().unwrap_or("")),
            "relay-remove" => self.remove_relay(f.get(1).copied().unwrap_or("")),
            "disconnect" => {
                if let Some(h) = &self.sh.st.lock().unwrap().hosting {
                    h.cut.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                host::close_sessions(net::CLOSE_DISCONNECTED);
                Ok(String::new())
            }
            "dismiss" => {
                let mut st = self.sh.st.lock().unwrap();
                if st.notice.as_ref().is_some_and(|(n, _)| f.get(1) == Some(&n.to_string().as_str())) {
                    st.notice = None;
                }
                Ok(String::new())
            }
            "unsuggest" => {
                self.sh.st.lock().unwrap().suggest = None;
                Ok(String::new())
            }
            other => Err(anyhow!("unknown request {other}")),
        };
        self.sh.changed();
        match r {
            Ok(s) => format!("ok\t{s}"),
            Err(e) => format!("err\t{e:#}"),
        }
    }

    /// The last `lines` lines of the service's log, which only admins can
    /// open directly. It holds no keys or tokens.
    fn log_tail(&self, lines: usize) -> Result<String> {
        if !self.sh.opts.service {
            return Err(anyhow!("only the service keeps its log out of reach"));
        }
        let text = std::fs::read(crate::service::data_dir().join("helper.log"))?;
        let text = String::from_utf8_lossy(&text);
        let all: Vec<&str> = text.lines().collect();
        Ok(all[all.len().saturating_sub(lines.min(5000))..].join("\n"))
    }

    /// The lobby window `window` closes or goes to the tray: that takes
    /// effect now, not `LOBBY_GONE` after its last snapshot.
    fn lobby_closing(&self, window: &str) -> String {
        let sh = &self.sh;
        let mut st = sh.st.lock().unwrap();
        if st.lobby_window.as_deref() == Some(window) {
            st.closed_window = Some(window.to_string());
            st.lobby_seen = None;
            sh.lobby_check(&mut st);
        }
        String::new()
    }

    /// Everything the lobby shows. `seen`: from the lobby window, which is
    /// open then; `window` names that window (its process and showing,
    /// `pid-n`), so a window closed and opened again within `LOBBY_GONE`
    /// still counts as closed.
    fn snapshot(&self, seen: bool, window: Option<&str>) -> String {
        let sh = &self.sh;
        let mut st = sh.st.lock().unwrap();
        if seen && (window.is_none() || window != st.closed_window.as_deref()) {
            if let Some(w) = window {
                if st.lobby_window.as_deref().is_some_and(|old| old != w && !process_alive(old)) && sh.lobby_open(&st) {
                    log!("the lobby window was opened again: connection codes void, temporary devices forgotten");
                    sh.forget_strangers(&mut st);
                }
                st.lobby_window = Some(w.to_string());
            }
            st.lobby_seen = Some(Instant::now());
            sh.lobby_check(&mut st);
        }
        let mut out = String::new();
        out += &format!("me\t{}\t{}\n", sh.me, sh.opts.service as u8);
        let s = &st.settings;
        out += &format!(
            "settings\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            s.shown_name(),
            s.name,
            s.public_relays as u8,
            s.lan_visible as u8,
            s.boot_online as u8,
            s.lock_control as u8,
            s.stay_awake as u8,
            s.language.as_str(),
        );
        // In the order of the list, the chosen relay first. home: in use;
        // temp: in use while the chosen one does not answer; idle: joined,
        // gets the guest list, not in use; fail: the last guest list did not
        // go through; wait: in use, not connected yet.
        let home = net::home_relay(&sh.ep());
        let cur = current_group(s, &st.using);
        for (i, g) in s.relays.iter().enumerate() {
            let ok = st.relay_ok.get(&g[0].to_string()).copied();
            let state = if ok == Some(false) {
                "fail"
            } else if cur.as_ref() != Some(g) {
                "idle"
            } else if home.as_ref().is_some_and(|h| g.contains(h)) && net::connected_to(&sh.ep(), g) {
                if i == 0 { "home" } else { "temp" }
            } else {
                "wait"
            };
            out += &format!("relay\t{}\t{state}\t\n", url_list(g));
        }
        if let Some(h) = &st.hosting {
            out += &format!("hosting\t{}\t{}\n", h.id, h.name);
        }
        if let Some(a) = &st.asking {
            out += &format!("asking\t{a}\n");
        }
        if let Some((n, t)) = &st.notice {
            out += &format!("notice\t{n}\t{}\n", t.replace(['\t', '\n'], " "));
        }
        if let Some(n) = &st.suggest {
            out += &format!("suggest\t{n}\n");
        }
        for c in st.codes.iter().filter(|c| c.by.is_none() && !c.expired()) {
            out += &format!("code\t{}\t{}\t{}\n", hex(&c.token), c.made.elapsed().as_secs(), c.text);
        }
        let open = sh.lobby_open(&st);
        let mut rows: Vec<String> = Vec::new();
        for d in st.store.listed() {
            rows.push(row(&st, &d.id, true, d.auto_accept, &self.sh.shown_name(&st, &d.id)));
        }
        for (id, p) in &st.peers {
            if st.store.get(id).is_some() || st.store.is_forgotten(id) || !open {
                continue;
            }
            let shown = match p.met {
                Some(Met::Code) => !matches!(p.status, Status::Refused(_)),
                Some(Met::Lan) => p.lan_active(),
                None => false,
            };
            if shown {
                rows.push(row(&st, id, false, false, &self.sh.shown_name(&st, id)));
            }
        }
        for r in rows {
            out += &r;
        }
        out
    }

    fn issue_code(&self) -> Result<String> {
        let mut token = [0u8; 8];
        getrandom::fill(&mut token).map_err(|e| anyhow!("random: {e}"))?;
        let (text, own) = {
            let mut st = self.sh.st.lock().unwrap();
            let home = net::home_relay(&self.sh.ep());
            let find = match (current_group(&st.settings, &st.using), home) {
                (Some(g), Some(h)) if g.contains(&h) => Find::Relay(first_of(&g, &h)),
                (Some(g), _) => Find::Relay(g),
                (None, Some(h)) => net::public_index(&h).map_or_else(|| Find::Relay(vec![h.clone()]), Find::Public),
                (None, None) => Find::Nowhere,
            };
            let code = ConnCode { device: self.sh.me, token, find, addrs: code_addrs(&self.sh.ep()) };
            let text = code.encode();
            log!("issued a connection code ({} addresses, {:?})", code.addrs.len(), code.find);
            let expires = rc_ident::proof::unix_now() + CODE_LIFETIME.as_secs();
            st.codes.push(Issued { token, text: text.clone(), made: Instant::now(), expires, by: None });
            (text, !st.settings.relays.is_empty())
        };
        if own {
            self.sh.send_invites_now();
        }
        Ok(text)
    }

    fn revoke(&self, token: &str) -> Result<String> {
        let t = unhex8(token).ok_or_else(|| anyhow!("bad token"))?;
        let own = {
            let mut st = self.sh.st.lock().unwrap();
            st.codes.retain(|c| c.token != t);
            !st.settings.relays.is_empty()
        };
        self.sh.guests_wake.notify_one();
        if own {
            self.sh.send_invites_now();
        }
        Ok(String::new())
    }

    /// A connection code someone sent: meet its sender.
    fn paste(&self, text: &str) -> Result<String> {
        let Some((code, _)) = ConnCode::find(text) else {
            if RelayCode::find(text).is_some() {
                return Err(anyhow!("{}", crate::i18n::tr(
                    "这是中转码，请贴到「设置 → 自建中转」里",
                    "This is a relay code; paste it under Settings → Private Relay",
                )));
            }
            return Err(anyhow!("{}", if text.contains("rc1") {
                crate::i18n::tr(
                    "连接码不完整或者有错字，请对方重新复制一次",
                    "Connection code is incomplete or has a typo; ask them to copy it again",
                )
            } else {
                crate::i18n::tr("这不是连接码", "This is not a connection code")
            }));
        };
        if code.device == self.sh.me {
            return Err(anyhow!("{}", crate::i18n::tr(
                "这是本机发出的连接码，要发给对方用",
                "This connection code is from this computer; send it to the other device",
            )));
        }
        let mut st = self.sh.st.lock().unwrap();
        // A fresh code cancels any undelivered delete of this device; otherwise
        // our next hello would send EVENT_FORGET instead of redeeming the token.
        if st.store.is_forgotten(&code.device) {
            st.store.remove(&code.device);
            self.sh.save(&mut st);
        }
        let p = st.peers.entry(code.device).or_insert_with(Peer::new);
        p.token = Some(code.token);
        p.code_addrs = code.addrs.clone();
        p.home = match &code.find {
            Find::Relay(u) => u.clone(),
            Find::Public(n) => net::public_relay(*n).into_iter().collect(),
            Find::Nowhere => p.home.clone(),
        };
        p.invite = match &code.find {
            Find::Relay(u) if u.iter().all(|u| net::public_index(u).is_none()) => Some(u.clone()),
            _ => None,
        };
        rc_relay::tls::learn_all(&p.home);
        // Found on the LAN before: its code makes it a device met with a code
        // (it stays when the LAN no longer shows it).
        p.met = Some(Met::Code);
        p.redial = p.redial.wrapping_add(1);
        // Drop any stale connection so peer_loop dials the new code's addresses
        // and relay right away instead of waiting for `hello` to time out.
        if let Some(old) = p.conn.take() {
            p.since = None;
            old.close(0u32.into(), b"redial");
        }
        p.status = Status::Checking;
        log!("pasted a connection code of {}", code.device.fmt_short());
        self.sh.ensure(&mut st, code.device);
        Ok(code.device.to_string())
    }

    fn rename(&self, id: EndpointId, name: &str) -> String {
        let mut st = self.sh.st.lock().unwrap();
        if let Some(d) = st.store.get_mut(&id) {
            d.name = clean(name);
            self.sh.save(&mut st);
        }
        String::new()
    }

    /// Deletes a device from the list, or a device met with a code; it is
    /// told (now, or when next reached) and forgets this computer too.
    fn remove(&self, id: EndpointId) -> String {
        self.sh.drop_guest(id, "deleted");
        String::new()
    }

    /// A relay code (`rcr1…`) pasted in the settings: this computer becomes
    /// an owner of that relay and uses it from now on. Waits for the relay.
    fn claim_relay(&self, text: &str) -> Result<String> {
        let Some(code) = RelayCode::find(text) else {
            return Err(anyhow!(
                "{}",
                if text.contains(rc_ident::relay_code::PREFIX) {
                    crate::i18n::tr(
                        "中转码不完整或者有错字，请重新复制一次",
                        "Relay code is incomplete or has a typo; please copy it again",
                    )
                } else {
                    crate::i18n::tr(
                        "这不是中转码（中转码以 rcr1 开头）",
                        "This is not a relay code (relay codes start with rcr1)",
                    )
                }
            ));
        };
        let ca = rc_relay::tls::ca();
        let name = self.sh.st.lock().unwrap().settings.shown_name();
        let claim = rc_relay::client::claim(&code, &self.sh.secret, &name, &ca);
        let urls = self.sh.rt.block_on(claim).map_err(relay_error)?;
        log!("joined the relay {}: using it from now on", url_list(&urls));
        self.choose_relay(urls.clone());
        Ok(url_list(&urls))
    }

    /// The own relay one of `urls` belongs to (all its addresses).
    fn own_relay(&self, urls: &str) -> Result<Vec<RelayUrl>> {
        let urls = relay_urls(urls);
        let st = self.sh.st.lock().unwrap();
        urls.iter().find_map(|u| group_of(&st.settings, u).cloned()).ok_or_else(|| {
            anyhow!("{}", crate::i18n::tr("还没有这个自建中转", "This private relay has not been added"))
        })
    }

    fn remove_relay(&self, urls: &str) -> Result<String> {
        let relay = self.own_relay(urls)?;
        self.settings(|s| {
            s.relays.retain(|g| *g != relay);
        });
        Ok(String::new())
    }

    /// 「使用」 on a relay of the list: it is the chosen one from now on.
    fn use_relay(&self, urls: &str) -> Result<String> {
        let relay = self.own_relay(urls)?;
        log!("chose the relay {}: using it from now on", url_list(&relay));
        self.choose_relay(relay);
        Ok(String::new())
    }

    /// Makes `relay` the chosen one (first in the list) and uses it now; the
    /// same relay at other addresses gives way to it.
    fn choose_relay(&self, relay: Vec<RelayUrl>) {
        let mut st = self.sh.st.lock().unwrap();
        let before = relays_of(&st.settings, &st.using);
        st.settings.relays.retain(|g| *g != relay && !g.iter().any(|u| relay.contains(u) || same_relay(u, &relay)));
        st.settings.relays.insert(0, relay);
        st.using = None;
        self.sh.settings_changed(&mut st, before);
    }

    /// 我的电脑 (`on`) or 信任设备: only what this computer lets it do.
    fn set_auto(&self, id: EndpointId, on: bool) -> String {
        let mut st = self.sh.st.lock().unwrap();
        if let Some(d) = st.store.get_mut(&id) {
            d.auto_accept = on;
            self.sh.save(&mut st);
            if on && !self.sh.opts.service && crate::install::service_running().is_none() {
                st.suggest = Some(self.sh.shown_name(&st, &id));
            }
            self.sh.ensure(&mut st, id);
        }
        String::new()
    }

    /// 「记住」 on a temporary device: asks it; the two remember each other
    /// once it agrees (or asked too). "both" when that is now.
    fn remember(&self, id: EndpointId) -> Result<String> {
        let mut st = self.sh.st.lock().unwrap();
        if st.store.get(&id).is_some() {
            return Ok("both".into());
        }
        let Some(p) = st.peers.get_mut(&id) else {
            return Err(anyhow!("{}", crate::i18n::tr("对方已经不在了", "The other device is no longer available")));
        };
        if p.ask == Ask::In {
            self.sh.befriend(&mut st, id, true);
            return Ok("both".into());
        }
        p.ask = Ask::Out;
        p.say_no = false;
        log!("asked {} to remember each other", id.fmt_short());
        self.sh.ensure(&mut st, id);
        Ok(String::new())
    }

    fn agree(&self, id: EndpointId) -> Result<String> {
        let mut st = self.sh.st.lock().unwrap();
        if st.peers.get(&id).is_none_or(|p| p.ask != Ask::In) {
            return Err(anyhow!("{}", crate::i18n::tr("对方的请求已经没了", "The request is no longer active")));
        }
        self.sh.befriend(&mut st, id, true);
        Ok(String::new())
    }

    fn refuse(&self, id: EndpointId) -> String {
        let mut st = self.sh.st.lock().unwrap();
        if let Some(p) = st.peers.get_mut(&id).filter(|p| p.ask == Ask::In) {
            p.ask = Ask::None;
            p.say_no = true;
            log!("did not agree to remember {}", id.fmt_short());
            self.sh.ensure(&mut st, id);
        }
        String::new()
    }

    fn cancel_remember(&self, id: EndpointId) -> String {
        let mut st = self.sh.st.lock().unwrap();
        if let Some(p) = st.peers.get_mut(&id).filter(|p| p.ask == Ask::Out) {
            p.ask = Ask::None;
            p.say_no = true;
            log!("cancelled remember request to {}", id.fmt_short());
            self.sh.ensure(&mut st, id);
        }
        String::new()
    }

    fn retry(&self, id: EndpointId) -> String {
        let mut st = self.sh.st.lock().unwrap();
        if let Some(p) = st.peers.get_mut(&id) {
            if let Some(c) = p.conn.take() {
                c.close(0u32.into(), b"retry");
            }
            p.status = Status::Checking;
            p.since = None;
        }
        self.sh.ensure(&mut st, id);
        String::new()
    }

    /// How a session window reaches `id`: addresses, relays to try, this
    /// computer's own relay, whether public relays are on.
    fn dial(&self, id: EndpointId) -> String {
        let st = self.sh.st.lock().unwrap();
        let (addrs, relays) = self.sh.dial_plan(&st, &id, 0);
        let mut addrs = addrs;
        if let Some(a) = st.peers.get(&id).and_then(|p| p.conn.as_ref()).and_then(net::direct_addr)
            && !addrs.contains(&a)
        {
            addrs.insert(0, a);
        }
        let list = |v: Vec<String>| v.join(",");
        // Video never goes through a public relay: when the other side is at
        // a self-hosted relay, a session has no use for them.
        let mut relays = relays;
        let home = st.peers.get(&id).map(|p| p.home.clone()).filter(|h| !h.is_empty());
        let home = home.or_else(|| st.store.get(&id).map(|d| d.relays.clone())).unwrap_or_default();
        if home.iter().any(|r| net::public_index(r).is_none()) {
            relays.retain(|r| net::public_index(r).is_none());
        }
        let own_relays: Vec<String> =
            current_group(&st.settings, &st.using).unwrap_or_default().iter().map(|r| r.to_string()).collect();
        format!(
            "{}\t{}\t{}\t{}\t{}",
            list(addrs.iter().map(|a| a.to_string()).collect()),
            list(relays.iter().map(|r| r.to_string()).collect()),
            list(own_relays),
            st.settings.public_relays as u8,
            self.sh.shown_name(&st, &id),
        )
    }

    fn settings(&self, f: impl FnOnce(&mut Settings)) -> String {
        let mut st = self.sh.st.lock().unwrap();
        let before = relays_of(&st.settings, &st.using);
        let before_boot = st.settings.boot_online;
        f(&mut st.settings);
        if st.using.as_ref().is_some_and(|g| !st.settings.relays.contains(g)) {
            st.using = None;
        }
        crate::power::STAY_AWAKE.store(st.settings.stay_awake, std::sync::atomic::Ordering::SeqCst);
        crate::i18n::set_pref(st.settings.language);
        if self.sh.opts.service
            && before_boot != st.settings.boot_online
            && let Err(e) = crate::install::set_auto_start(st.settings.boot_online)
        {
            log!("set auto start: {e:#}");
        }
        self.sh.settings_changed(&mut st, before);
        String::new()
    }
}

async fn apply_relays(ep: &Endpoint, before: &net::Relays, after: &net::Relays) {
    let (old, new) = (map_urls(before), map_urls(after));
    for u in old.iter().filter(|u| !new.contains(u)) {
        ep.remove_relay(u).await;
    }
    for u in new.iter().filter(|u| !old.contains(u)) {
        ep.insert_relay(u.clone(), Arc::new(RelayConfig::from(u.clone()))).await;
    }
    log!("relays now: {}", new.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", "));
}

/// Whether `u` is an address of the relay at `urls`: the same certificate pin.
fn same_relay(u: &RelayUrl, urls: &[RelayUrl]) -> bool {
    rc_relay::tls::pin_of(u).is_some_and(|p| urls.iter().any(|v| rc_relay::tls::pin_of(v) == Some(p)))
}

/// What a failed call to the own relay means to the user.
fn relay_error(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<Refusal>() {
        Some(Refusal::UnknownKey) => anyhow!("{}", crate::i18n::tr(
            "这个中转码已经在中转上删掉了，请向中转的管理者要一个新的",
            "This relay code was deleted on the relay; ask the relay admin for a new one",
        )),
        Some(Refusal::NotOwner) => anyhow!("{}", crate::i18n::tr(
            "这台电脑已经被移出这台中转，请重新贴一次中转码",
            "This computer was removed from this relay; please paste a relay code again",
        )),
        Some(Refusal::Clock) => anyhow!("{}", crate::i18n::tr(
            "这台电脑的时间和中转相差太多，请先校准时间",
            "This computer's clock differs too much from the relay; please sync your clock",
        )),
        Some(r) => {
            if crate::i18n::is_en() {
                anyhow!("Relay refused ({})", r.word())
            } else {
                anyhow!("中转拒绝了（{}）", r.word())
            }
        }
        None => {
            let prefix = crate::i18n::tr("连不上中转：", "Cannot reach relay: ");
            anyhow!("{prefix}{e:#}")
        }
    }
}

/// One `dev` line of the snapshot.
fn row(st: &St, id: &EndpointId, listed: bool, auto: bool, name: &str) -> String {
    let p = st.peers.get(id);
    let (status, rtt, path) = match p.map_or(Status::Checking, |p| p.status) {
        Status::Checking => ("checking".to_string(), 0.0, 0),
        Status::Online { rtt_ms, path } => ("online".to_string(), rtt_ms, path.code()),
        Status::Offline => ("offline".to_string(), 0.0, 0),
        Status::Unreachable => ("unreachable".to_string(), 0.0, 0),
        Status::Refused(r) => (format!("refused:{r}"), 0.0, 0),
    };
    let met = match p.and_then(|p| p.met) {
        Some(Met::Lan) => "lan",
        Some(Met::Code) => "code",
        None => "-",
    };
    let lan = p.is_some_and(|p| p.on_lan()) as u8;
    let ask = match p.map_or(Ask::None, |p| p.ask) {
        Ask::Out => "out",
        Ask::In => "in",
        Ask::None => "-",
    };
    format!("dev\t{id}\t{}\t{}\t{met}\t{status}\t{rtt:.1}\t{path}\t{lan}\t{name}\t{ask}\n", listed as u8, auto as u8)
}

/// Keeps a meet connection to `id` while it should have a card (see the module comment).
async fn peer_loop(sh: Arc<Shared>, id: EndpointId, wake: Arc<Notify>) {
    let mut round = 0u32;
    loop {
        let (addrs, relays, invite, redial) = {
            let mut st = sh.st.lock().unwrap();
            if !sh.wanted(&st, &id) {
                if let Some(p) = st.peers.get_mut(&id) {
                    p.running = false;
                    if p.met == Some(Met::Lan) {
                        p.met = None;
                    }
                    if let Some(c) = p.conn.take() {
                        c.close(0u32.into(), b"forgotten");
                    }
                }
                st.peers.retain(|k, p| *k != id || p.met.is_some() || p.on_lan());
                drop(st);
                sh.changed();
                return;
            }
            let (addrs, relays) = sh.dial_plan(&st, &id, round);
            (
                addrs,
                relays,
                st.peers.get(&id).and_then(|p| Some((p.invite.clone()?, p.token?))),
                st.peers.get(&id).map_or(0, |p| p.redial),
            )
        };
        if let Some((relay, token)) = invite {
            sh.bring_code(id, &relay, token, round).await;
        }
        round += 1;
        let target = net::dial_addr(id, &addrs, &relays);
        #[cfg(test)]
        let blocked = live::NO_DIAL.lock().unwrap().contains(&sh.me);
        #[cfg(not(test))]
        let blocked = false;
        let mut dial = std::pin::pin!(tokio::time::timeout(CONNECT_WAIT, async {
            if blocked {
                std::future::pending().await
            } else {
                sh.ep().connect(target, meet::ALPN).await
            }
        }));
        let r = loop {
            tokio::select! {
                res = &mut dial => break Some(res),
                _ = wake.notified() => {
                    if sh.st.lock().unwrap().peers.get(&id).is_some_and(|p| p.redial != redial) {
                        break None;
                    }
                }
            }
        };
        let Some(r) = r else {
            round = 0;
            continue;
        };
        let conn = match r {
            Ok(Ok(c)) => c,
            other => {
                let why = match other {
                    Ok(Err(e)) => format!("{e:#}"),
                    _ => format!("no answer within {} s", CONNECT_WAIT.as_secs()),
                };
                if round <= 2 || round.is_multiple_of(20) {
                    log!("{} not reached ({} addresses, {} relays): {why}", id.fmt_short(), addrs.len(), relays.len());
                }
                {
                    let mut st = sh.st.lock().unwrap();
                    // A deleted device that stays away is told next run, not every 15 s forever.
                    let give_up = round >= 4 && st.store.is_forgotten(&id);
                    if let Some(p) = st.peers.get_mut(&id) {
                        // Its own connection to this computer still shows the card.
                        if p.conn.as_ref().is_none_or(|c| c.close_reason().is_some()) {
                            p.status = Status::Offline;
                            p.conn = None;
                            p.since = None;
                        }
                        if give_up {
                            p.running = false;
                        }
                    }
                    if give_up {
                        return;
                    }
                }
                sh.changed();
                let retry = {
                    let st = sh.st.lock().unwrap();
                    if st.peers.get(&id).is_some_and(|p| p.redial != redial) {
                        Duration::ZERO
                    } else if round == 1 && st.peers.get(&id).is_some_and(|p| p.met == Some(Met::Code) && p.token.is_some()) {
                        Duration::from_secs(2)
                    } else {
                        REFIND_EVERY
                    }
                };
                if !retry.is_zero() {
                    let _ = tokio::time::timeout(retry, wake.notified()).await;
                }
                if sh.st.lock().unwrap().peers.get(&id).is_some_and(|p| p.redial != redial) {
                    round = 0;
                }
                continue;
            }
        };
        round = 0;
        sh.track(&conn);
        let refused = talk(&sh, id, &conn, &wake).await;
        let gone = departed(&conn);
        let forgotten = !sh.wanted(&sh.st.lock().unwrap(), &id);
        conn.close(0u32.into(), if forgotten { b"forgotten" } else { b"bye" });
        {
            let mut st = sh.st.lock().unwrap();
            let listed = st.store.get(&id).is_some();
            if let Some(p) = st.peers.get_mut(&id) {
                if p.conn.as_ref().is_some_and(|c| c.stable_id() == conn.stable_id()) {
                    p.conn = None;
                    p.since = None;
                    if !refused {
                        p.status = Status::Offline;
                    }
                }
                if gone && !listed {
                    p.lan = None;
                    p.met = None;
                }
            }
        }
        sh.changed();
        if !refused && sh.st.lock().unwrap().peers.get(&id).is_some_and(|p| p.redial != redial || p.token.is_some()) {
            round = 0;
            continue;
        }
        let pause = if refused { REFIND_EVERY } else { Duration::from_secs(1) };
        let _ = tokio::time::timeout(pause, wake.notified()).await;
    }
}

fn is_lan_temp(st: &St, id: &EndpointId) -> bool {
    !st.store.devices.iter().any(|d| d.id == *id)
        && st.peers.get(id).is_some_and(|p| p.met == Some(Met::Lan))
}

/// Says hello on `conn` (again whenever woken, and every `HELLO_AGAIN`
/// status rounds, or `LAN_HELLO_AGAIN` for LAN-only temporary peers), keeping
/// the status fresh, until the connection ends. Returns whether the other side refused.
async fn talk(sh: &Arc<Shared>, id: EndpointId, conn: &Connection, wake: &Notify) -> bool {
    loop {
        let (hi, sent, lan_temp) = {
            let st = sh.st.lock().unwrap();
            if !sh.wanted(&st, &id) {
                return false;
            }
            let (hi, sent) = sh.hello(&st, &id);
            (hi, sent, is_lan_temp(&st, &id))
        };
        let reply = if lan_temp {
            meet::ask_within(conn, &hi, LAN_ASK_WAIT).await
        } else {
            meet::ask(conn, &hi).await
        };
        match reply {
            Ok(Msg::MeetWelcome { name, home, event, .. }) => {
                let (first, done) = {
                    let mut st = sh.st.lock().unwrap();
                    let first = sent != meet::EVENT_FORGET && st.peers.get(&id).is_none_or(|p| p.conn.is_none());
                    sh.welcomed(&mut st, &id, conn, name.clone(), home, sent, event);
                    (first, sent == meet::EVENT_FORGET || !sh.wanted(&st, &id))
                };
                if first {
                    let (k, addr, rtt) = net::selected_path(conn);
                    log!("met {} ({name}) via {} {addr}, {rtt:.0} ms", id.fmt_short(), k.label());
                }
                sh.changed();
                if done {
                    return false;
                }
            }
            Ok(Msg::MeetRefuse { reason }) => {
                let mut st = sh.st.lock().unwrap();
                sh.refused(&mut st, &id, reason, sent);
                drop(st);
                sh.changed();
                return true;
            }
            Ok(_) => return false,
            Err(e) => {
                log!("hello to {}: {e:#}", id.fmt_short());
                return false;
            }
        }
        // Hello again now and then: when only this side can call, the other
        // side's news (a request to remember) comes back in the welcome.
        let mut rounds = 0u32;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(STATUS_EVERY) => {
                    let mut st = sh.st.lock().unwrap();
                    if !sh.wanted(&st, &id) {
                        return false;
                    }
                    sh.path_status(&mut st, &id);
                    let every = if is_lan_temp(&st, &id) { LAN_HELLO_AGAIN } else { HELLO_AGAIN };
                    drop(st);
                    sh.changed();
                    rounds += 1;
                    if rounds >= every {
                        break;
                    }
                }
                _ = wake.notified() => break,
                _ = conn.closed() => return false,
            }
        }
    }
}

/// Takes the connections of the endpoint `ep` while it is the current one.
async fn accept_loop(sh: Arc<Shared>, ep: Endpoint) {
    while let Some(incoming) = ep.accept().await {
        let sh = sh.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => return log!("incoming connection failed: {e:#}"),
            };
            if conn.alpn() == meet::ALPN {
                serve_meet(sh, conn).await;
            } else if conn.alpn() == proto::ALPN {
                serve_desktop(sh, conn).await;
            } else if conn.alpn() == term::ALPN {
                serve_term(sh, conn).await;
            } else {
                conn.close(0u32.into(), b"unknown protocol");
            }
        });
    }
}

/// Whether the other side closed `conn` because it went away or forgot
/// this computer, not for a moment (a new endpoint, a failed hello): a
/// LAN card then goes now instead of `LAN_FORGET` after the last answer.
fn departed(conn: &Connection) -> bool {
    matches!(conn.close_reason(), Some(iroh::endpoint::ConnectionError::ApplicationClosed(c))
        if c.error_code.into_inner() == u64::from(net::CLOSE_STOPPED) || &c.reason[..] == b"forgotten")
}

async fn serve_meet(sh: Arc<Shared>, conn: Connection) {
    let from = conn.remote_id();
    if sh.leaving.load(std::sync::atomic::Ordering::SeqCst) {
        return conn.close(net::CLOSE_STOPPED.into(), b"leaving");
    }
    sh.track(&conn);
    // Ends with the connection, by itself: it takes the card offline then.
    let mut watching = false;
    loop {
        match meet::next_hi(&conn).await {
            Ok(Some((Msg::MeetHi { name, home, token, event, .. }, send))) => {
                let forget = event == meet::EVENT_FORGET && token.is_none();
                let reply = sh.on_hi(from, &conn, name, home, token, event);
                let welcomed = matches!(reply, Msg::MeetWelcome { .. });
                if let Err(e) = meet::answer(send, &reply).await {
                    log!("answer to {}: {e:#}", from.fmt_short());
                    break;
                }
                if forget {
                    break;
                }
                if welcomed && !watching {
                    watching = true;
                    tokio::spawn(watch_incoming(sh.clone(), conn.clone()));
                }
            }
            Ok(_) => break,
            Err(e) => {
                log!("meet connection from {}: {e:#}", from.fmt_short());
                break;
            }
        }
    }
}

/// A meet connection the other device opened. While this computer has no
/// connection of its own to it (its calls may not get through when only the
/// other side can reach out), the card shows this one, so both sides show
/// each other as long as either connection holds.
///
/// A device forgotten here (deleted, or a temporary one when the lobby
/// closed) loses this connection too: it dials again, is refused and drops
/// its card, instead of showing this computer online.
async fn watch_incoming(sh: Arc<Shared>, conn: Connection) {
    let from = conn.remote_id();
    let mine = |p: &Peer| p.conn.as_ref().is_some_and(|c| c.stable_id() == conn.stable_id());
    loop {
        {
            let mut st = sh.st.lock().unwrap();
            let lan_ok = st.peers.get(&from).is_some_and(|p| p.met == Some(Met::Lan)) && is_lan_conn(&conn);
            if sh.is_stopped() || !(sh.knows(&st, &from) || sh.lobby_open(&st) && lan_ok) {
                drop(st);
                conn.close(0u32.into(), b"forgotten");
                break;
            }
            if let Some(p) = st.peers.get_mut(&from) {
                if conn.close_reason().is_none() && p.conn.as_ref().is_none_or(|c| c.close_reason().is_some()) {
                    p.conn = Some(conn.clone());
                    p.since = Some(Instant::now());
                    let (k, addr, rtt) = net::selected_path(&conn);
                    log!("{} reached this computer: shown over its connection, {} {addr}, {rtt:.0} ms", from.fmt_short(), k.label());
                }
                if mine(p) {
                    sh.path_status(&mut st, &from);
                    drop(st);
                    sh.changed();
                }
            }
        }
        tokio::select! {
            _ = conn.closed() => break,
            _ = tokio::time::sleep(STATUS_EVERY) => {}
        }
    }
    let mut st = sh.st.lock().unwrap();
    if departed(&conn)
        && st.store.get(&from).is_none()
        && let Some(p) = st.peers.get_mut(&from)
    {
        // It forgot this computer: only a new code or the LAN brings it back.
        p.lan = None;
        p.met = None;
    }
    if st.peers.get(&from).is_some_and(mine) {
        if let Some(p) = st.peers.get_mut(&from) {
            p.conn = None;
            p.since = None;
            p.status = Status::Offline;
        }
        sh.ensure(&mut st, from);
        drop(st);
        sh.changed();
    }
}

/// A desktop session request: who it is (its proof), whether it may, then the session.
async fn serve_desktop(sh: Arc<Shared>, conn: Connection) {
    let remote = conn.remote_id();
    let streams = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi()).await;
    let Ok(Ok((mut send, mut recv))) = streams else {
        return log!("desktop connection from {}: no control stream", remote.fmt_short());
    };
    let hello = match tokio::time::timeout(Duration::from_secs(5), read_msg(&mut recv)).await {
        Ok(Ok(Msg::Hello { max_kbps, fps, caps, proof, quality })) => (host::Hello { max_kbps, fps, caps, quality }, proof),
        _ => {
            conn.close(net::CLOSE_NOT_MET.into(), b"expected hello");
            return;
        }
    };
    let device = match ident::identify(remote, &hello.1) {
        Ok(d) => d,
        Err(e) => {
            log!("desktop connection from {}: {e:#}", remote.fmt_short());
            conn.close(net::CLOSE_NOT_MET.into(), b"bad proof");
            return;
        }
    };
    let name = match admit(&sh, device, &conn, &mut send, &Msg::Waiting.encode()).await {
        Ok(n) => n,
        Err(code) => {
            log!("refused a session from {} (close code {code})", device.fmt_short());
            conn.close(code.into(), b"refused");
            return;
        }
    };
    log!("{name} ({}) controls this computer now", device.fmt_short());
    sh.changed();
    let r = host::session(conn, send, recv, hello.0, sh.opts.pipe, sh.opts.service).await;
    match r {
        Ok(()) => log!("{name} left"),
        Err(e) => log!("session with {name} ended: {e:#}"),
    }
    end_use(&mut sh.st.lock().unwrap(), false);
    sh.changed();
}

/// A shell, command or copy (`term`): who it is, whether it may (as for a desktop session), then the request.
async fn serve_term(sh: Arc<Shared>, conn: Connection) {
    let remote = conn.remote_id();
    let Ok(Ok((mut send, mut recv))) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi()).await else {
        return log!("terminal connection from {}: no stream", remote.fmt_short());
    };
    let open = match tokio::time::timeout(Duration::from_secs(5), term::read_frame(&mut recv)).await {
        Ok(Ok(Some((term::OPEN, b)))) => term::Open::decode(&b).ok(),
        _ => None,
    };
    let Some(open) = open else {
        return conn.close(net::CLOSE_NOT_MET.into(), b"expected open");
    };
    let device = match ident::identify(remote, &open.proof) {
        Ok(d) => d,
        Err(e) => {
            log!("terminal connection from {}: {e:#}", remote.fmt_short());
            return conn.close(net::CLOSE_NOT_MET.into(), b"bad proof");
        }
    };
    let name = match admit(&sh, device, &conn, &mut send, &term::frame(term::WAITING, &[])).await {
        Ok(n) => n,
        Err(code) => {
            log!("refused a terminal request from {} (close code {code})", device.fmt_short());
            return conn.close(code.into(), b"refused");
        }
    };
    let what = open.what;
    log!("{name} ({}): {what:?}{}", device.fmt_short(), if what == term::What::Exec { format!(" {:?}", open.text) } else { String::new() });
    sh.changed();
    host::track(&conn);
    let r = term::serve(open, conn.clone(), send, recv, sh.opts.service).await;
    host::untrack(&conn);
    if let Err(e) = r {
        log!("{what:?} for {name}: {e:#}");
    }
    // The client closes once it has everything; the stream's end is not that.
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
    end_use(&mut sh.st.lock().unwrap(), what != term::What::Shell);
    sh.changed();
}

/// Whether `device` may control this computer now (`target-plan` 2, 6):
/// "my computer" goes in; otherwise the user is asked (one popup at a time,
/// not on the lock screen). On success the
/// session is marked as hosted and its bar shows. Errors are close codes.
async fn admit(sh: &Arc<Shared>, device: EndpointId, conn: &Connection, send: &mut iroh::endpoint::SendStream, waiting: &[u8]) -> Result<String, u32> {
    let (kind, name) = {
        let mut st = sh.st.lock().unwrap();
        if device == sh.me {
            return Err(net::CLOSE_NOT_MET);
        }
        let name = sh.shown_name(&st, &device);
        match &st.hosting {
            Some(h) if h.id != device => return Err(net::CLOSE_BUSY),
            // Already let in: one more window, shell or command.
            Some(_) => {
                start_hosting(sh, &mut st, device, &name);
                return Ok(name);
            }
            None => {}
        }
        if st.handoff_host.as_ref().is_some_and(|(hid, _, deadline)| *hid == device && Instant::now() < *deadline) {
            let (_, hname, _) = st.handoff_host.take().unwrap();
            let shown = if hname.is_empty() { name } else { hname };
            start_hosting(sh, &mut st, device, &shown);
            log!("resumed hosting {shown} ({}) after service handoff", device.fmt_short());
            return Ok(shown);
        }
        let listed = st.store.get(&device).cloned();
        if !st.settings.lock_control && !desktop::user_desktop_shown() {
            return Err(net::CLOSE_LOCKED);
        }
        if listed.as_ref().is_some_and(|d| d.auto_accept) {
            start_hosting(sh, &mut st, device, &name);
            return Ok(name);
        }
        if !desktop::user_desktop_shown() {
            return Err(net::CLOSE_LOCKED);
        }
        let met = if sh.lobby_open(&st) { st.peers.get(&device).and_then(|p| p.met) } else { None };
        let kind = match (&listed, met) {
            (Some(_), _) => Kind::Trusted,
            (None, Some(m)) => Kind::New { via_code: m == Met::Code },
            (None, None) => return Err(net::CLOSE_NOT_MET),
        };
        if st.asking.is_some() {
            return Err(net::CLOSE_BUSY);
        }
        st.asking = Some(name.clone());
        (kind, name)
    };
    sh.changed();
    let _ = send.write_all(waiting).await;
    log!("asking whether {name} ({}) may control this computer", device.fmt_short());
    let mut asking = consent::ask(&name, kind, ASK_WAIT);
    let answer = tokio::select! {
        a = &mut asking.rx => a.unwrap_or(Answer::Cancelled),
        _ = conn.closed() => {
            asking.cancel();
            Answer::Cancelled
        }
        _ = tokio::time::sleep(ASK_WAIT + Duration::from_secs(2)) => {
            asking.cancel();
            Answer::NoAnswer
        }
    };
    log!("answer for {name}: {answer:?}");
    let mut st = sh.st.lock().unwrap();
    st.asking = None;
    match answer {
        Answer::Deny => return Err(net::CLOSE_DENIED),
        Answer::NoAnswer | Answer::Cancelled => return Err(net::CLOSE_NO_ANSWER),
        Answer::Allow => {}
    }
    if conn.close_reason().is_some() {
        return Err(net::CLOSE_NO_ANSWER);
    }
    if st.hosting.as_ref().is_some_and(|h| h.id != device) {
        return Err(net::CLOSE_BUSY);
    }
    start_hosting(sh, &mut st, device, &name);
    Ok(name)
}

fn start_hosting(_sh: &Arc<Shared>, st: &mut St, id: EndpointId, name: &str) {
    if let Some(h) = st.hosting.as_mut().filter(|h| h.id == id) {
        h.uses += 1;
        h.idle_since = None;
        return;
    }
    crate::power::wake_screen();
    let cut = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bar = {
        let cut = cut.clone();
        ctlbar::show(name, move || {
            cut.store(true, std::sync::atomic::Ordering::SeqCst);
            host::close_sessions(net::CLOSE_DISCONNECTED);
        })
    };
    st.hosting = Some(Hosting { id, name: name.to_string(), _bar: bar, uses: 1, idle_since: None, cut });
}

/// One use of the hosting ended. `linger`: it was a command or a copy, so
/// the permission stays `LINGER` for the next one.
fn end_use(st: &mut St, linger: bool) {
    let Some(h) = st.hosting.as_mut() else { return };
    h.uses = h.uses.saturating_sub(1);
    if h.uses > 0 {
        return;
    }
    if linger && !h.cut.load(std::sync::atomic::Ordering::SeqCst) {
        h.idle_since = Some(Instant::now());
    } else {
        st.hosting = None;
    }
}

/// Clears a hosting nothing uses any more (`LINGER` over, or 【断开】).
fn hosting_over(st: &mut St) -> bool {
    let over = st.hosting.as_ref().is_some_and(|h| {
        h.uses == 0 && (h.cut.load(std::sync::atomic::Ordering::SeqCst) || h.idle_since.is_none_or(|t| t.elapsed() > LINGER))
    });
    if over {
        st.hosting = None;
    }
    over
}

/// Looks for devices on the LAN while the lobby is open.
async fn discovery_loop(sh: Arc<Shared>) {
    let mut logged = BTreeSet::new();
    loop {
        if sh.is_stopped() {
            return;
        }
        if !sh.lobby_open(&sh.st.lock().unwrap()) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let found = match discovery::scan(Duration::from_millis(1500), &sh.me).await {
            Ok(f) => f,
            Err(e) => {
                log!("LAN search: {e:#}");
                Vec::new()
            }
        };
        {
            let mut st = sh.st.lock().unwrap();
            for f in found {
                if logged.insert(f.id) {
                    log!("found {} ({}) on the LAN at {}", f.name, f.id.fmt_short(), f.addr);
                }
                let forgotten = st.store.is_forgotten(&f.id);
                let p = st.peers.entry(f.id).or_insert_with(Peer::new);
                let was_seen = p.on_lan();
                p.lan = Some((f.addr, Instant::now()));
                if p.name.is_empty() {
                    p.name = clean(&f.name);
                }
                if p.met.is_none() && !forgotten {
                    p.met = Some(Met::Lan);
                }
                // Just appeared: try it now rather than at the next round.
                let kick = (!was_seen && !matches!(p.status, Status::Online { .. })) || !p.running;
                if kick {
                    sh.ensure(&mut st, f.id);
                }
            }
        }
        sh.changed();
        tokio::time::sleep(DISCOVERY_EVERY.saturating_sub(Duration::from_millis(1500))).await;
    }
}

/// Once a second: codes that expired, and in service mode what follows from
/// the lobby window closing (`target-plan` 6.5): codes void, temporary
/// devices forgotten.
async fn housekeeping(sh: Arc<Shared>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if sh.is_stopped() {
            return;
        }
        let mut st = sh.st.lock().unwrap();
        st.codes.retain(|c| c.by.is_some() || c.made.elapsed() < CODE_KEEP);
        if st.handoff_host.as_ref().is_some_and(|(_, _, deadline)| Instant::now() >= *deadline) {
            st.handoff_host = None;
        }
        let over = hosting_over(&mut st);
        let lapsed = lapse_asks(&sh, &mut st);
        if sh.lobby_check(&mut st) || over || lapsed {
            drop(st);
            sh.changed();
        }
    }
}

/// Whether the process with this id (as text, from the lobby) still runs.
/// Two lobby windows that both still run are not one reopened.
fn process_alive(pid: &str) -> bool {
    use windows::Win32::{
        Foundation::{CloseHandle, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let Ok(pid) = pid.split('-').next().unwrap_or(pid).parse::<u32>() else { return false };
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        alive
    }
}

/// What follows from the lobby window closing in service mode.
fn lobby_closed(st: &mut St) {
    st.codes.clear();
    for p in st.peers.values_mut() {
        p.met = None;
        p.token = None;
        p.invite = None;
        p.ask = Ask::None;
        p.say_no = false;
        p.wake.notify_one();
    }
}

/// Requests to remember each other that are over: the two remember each
/// other now, or the other side went away without answering (its window
/// closed), which this computer's user is told.
fn lapse_asks(sh: &Shared, st: &mut St) -> bool {
    let open = sh.lobby_open(st);
    let mut unanswered = Vec::new();
    let mut any = false;
    for (id, p) in st.peers.iter_mut() {
        if p.ask == Ask::None && !p.say_no {
            continue;
        }
        let listed = st.store.get(id).is_some();
        if listed || p.met.is_none() || !open {
            if p.ask == Ask::Out && !listed && open {
                unanswered.push(*id);
            }
            p.ask = Ask::None;
            p.say_no = false;
            any = true;
        }
    }
    for id in unanswered {
        let name = sh.shown_name(st, &id);
        log!("{} ({name}) went away without answering the request to remember each other", id.fmt_short());
        let msg = if crate::i18n::is_en() {
            format!("\"{name}\" did not respond")
        } else {
            format!("「{name}」没有回应")
        };
        sh.notice(st, msg);
    }
    any
}

/// One list `guests_loop` keeps on one relay.
struct Track<T> {
    sent: Option<T>,
    synced_at: Option<Instant>,
    retry_at: Option<Instant>,
    last_error: String,
}

impl<T: Clone + PartialEq> Track<T> {
    fn new(sent: Option<T>) -> Self {
        Track { sent, synced_at: None, retry_at: None, last_error: String::new() }
    }

    /// Whether `list` should go out now (`refresh` also re-syncs periodically
    /// when unchanged so relay-side kicks reach this computer).
    fn due(&self, list: &T, refresh: Option<Duration>) -> bool {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        self.sent.as_ref() != Some(list)
            || refresh.is_some_and(|every| self.synced_at.is_none_or(|at| at.elapsed() >= every))
    }

    /// Takes the outcome of sending `list`; true if it went through.
    fn note(&mut self, r: Result<()>, relay: &[RelayUrl], what: &str, list: &T) -> bool {
        const RETRY: Duration = Duration::from_secs(30);
        match r {
            Ok(()) => {
                self.sent = Some(list.clone());
                self.synced_at = Some(Instant::now());
                self.retry_at = None;
                self.last_error.clear();
                true
            }
            Err(e) => {
                let e = format!("{e:#}");
                if e != self.last_error {
                    log!("{what} on {}: {e}; trying again every {} s", relay[0], RETRY.as_secs());
                    self.last_error = e;
                }
                self.retry_at = Some(Instant::now() + RETRY);
                false
            }
        }
    }
}

/// What `guests_loop` told one own relay.
struct RelayTrack {
    guests: Track<Vec<Guest>>,
    invites: Track<Vec<Invite>>,
}

/// Keeps this computer's guest list and connection codes on every own
/// relay: each whole list goes out at start (the codes only once there are
/// any), as soon as it changes, and when the relay's watch (`watch_relay`)
/// asks, which it does at once when a guest is kicked there: the answer
/// names the kicked guests, they are dropped here and the shorter list goes
/// to every relay. A relay too old for a watch gets the list every
/// `GUEST_SYNC_EVERY` instead. Again 30 s after a failure.
async fn guests_loop(sh: Arc<Shared>) {
    /// Looked at again this long after the last change at the latest.
    const EVERY: Duration = Duration::from_secs(2);
    let mut tracked: BTreeMap<Vec<RelayUrl>, RelayTrack> = BTreeMap::new();
    let mut watches: BTreeMap<Vec<RelayUrl>, tokio::task::JoinHandle<()>> = BTreeMap::new();
    loop {
        if sh.is_stopped() || sh.leaving.load(std::sync::atomic::Ordering::SeqCst) {
            for (_, w) in watches {
                w.abort();
            }
            return;
        }
        let (relays, mut guests, invites) = {
            let st = sh.st.lock().unwrap();
            (st.settings.relays.clone(), sh.guests(&st), sh.invites(&st))
        };
        let removed: Vec<Vec<RelayUrl>> = tracked.keys().filter(|r| !relays.contains(r)).cloned().collect();
        for old in removed {
            // The same relay at other addresses (`choose_relay`) gets the
            // lists below: empty ones first would cut its guests off meanwhile.
            let moved = relays.iter().any(|g| old.iter().any(|u| same_relay(u, g)));
            if !moved {
                let _ = tokio::time::timeout(Duration::from_secs(2), sh.send_guests(&old, &[])).await;
                if tracked[&old].invites.sent.as_ref().is_none_or(|i| !i.is_empty()) {
                    let _ = tokio::time::timeout(Duration::from_secs(2), sh.send_invites(&old, &[])).await;
                }
            }
            tracked.remove(&old);
            sh.st.lock().unwrap().relay_ok.remove(&old[0].to_string());
        }
        watches.retain(|r, w| {
            let keep = relays.contains(r);
            if !keep {
                w.abort();
                sh.watching.lock().unwrap().remove(r);
            }
            keep
        });
        for relay in &relays {
            watches.entry(relay.clone()).or_insert_with(|| tokio::spawn(watch_relay(sh.clone(), relay.clone())));
        }
        for relay in &relays {
            let RelayTrack { guests: g, invites: i } = tracked.entry(relay.clone()).or_insert_with(|| RelayTrack { guests: Track::new(None), invites: Track::new(Some(Vec::new())) });
            let asked = sh.resync.lock().unwrap().remove(relay);
            let refresh = (!sh.watching.lock().unwrap().contains(relay)).then_some(GUEST_SYNC_EVERY);
            if asked || g.due(&guests, refresh) {
                let r = sh.send_guests(relay, &guests).await;
                let dropped = r.as_ref().ok().cloned().unwrap_or_default();
                let ok = g.note(r.map(|_| ()).map_err(relay_error), relay, "guest list", &guests);
                sh.st.lock().unwrap().relay_ok.insert(relay[0].to_string(), ok);
                if !dropped.is_empty() {
                    let mut changed = false;
                    for id in dropped {
                        changed |= sh.drop_guest(id, "kicked on relay");
                    }
                    // The relays before this one in the list get the shorter list too.
                    sh.guests_wake.notify_one();
                    if changed {
                        guests = {
                            let st = sh.st.lock().unwrap();
                            sh.guests(&st)
                        };
                        let r2 = sh.send_guests(relay, &guests).await;
                        let ok2 = g.note(r2.map(|_| ()).map_err(relay_error), relay, "guest list", &guests);
                        sh.st.lock().unwrap().relay_ok.insert(relay[0].to_string(), ok2);
                    }
                }
                sh.changed();
            }
            if i.due(&invites, None) {
                i.note(sh.send_invites(relay, &invites).await, relay, "connection codes", &invites);
            }
        }
        let _ = tokio::time::timeout(EVERY, sh.guests_wake.notified()).await;
    }
}

/// Without a watch, how often the guest list goes to a relay to hear of kicks.
const GUEST_SYNC_EVERY: Duration = Duration::from_secs(15);

/// Keeps this computer's watch open on one own relay (see `rc_relay::api`):
/// each `sync` from it has `guests_loop` send the guest list now. Opened
/// again a second after it ends (the first `sync` of the new one covers what
/// came in between), more slowly while opening keeps failing; on a relay too
/// old for it, tried again every 10 minutes.
async fn watch_relay(sh: Arc<Shared>, relay: Vec<RelayUrl>) {
    const NO_WATCH_RETRY: Duration = Duration::from_secs(600);
    let mut wait = Duration::from_secs(1);
    let mut last_err = String::new();
    loop {
        if sh.is_stopped() {
            return;
        }
        let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sync = {
            let (sh, relay, open) = (sh.clone(), relay.clone(), open.clone());
            move || {
                open.store(true, std::sync::atomic::Ordering::SeqCst);
                sh.watching.lock().unwrap().insert(relay.clone());
                sh.resync.lock().unwrap().insert(relay.clone());
                sh.guests_wake.notify_one();
            }
        };
        let r = rc_relay::client::watch(&relay, &sh.secret, &rc_relay::tls::ca(), sync).await;
        sh.watching.lock().unwrap().remove(&relay);
        if open.load(std::sync::atomic::Ordering::SeqCst) {
            wait = Duration::from_secs(1);
        }
        match r {
            Err(e) if e.downcast_ref::<rc_relay::client::NoWatch>().is_some() => {
                if last_err != "no watch" {
                    log!("{}: {e}; sending the guest list every {} s", relay[0], GUEST_SYNC_EVERY.as_secs());
                    last_err = "no watch".into();
                }
                tokio::time::sleep(NO_WATCH_RETRY).await;
                continue;
            }
            Err(e) => {
                let e = format!("{:#}", relay_error(e));
                if e != last_err {
                    log!("watch on {}: {e}; opening it again", relay[0]);
                    last_err = e;
                }
            }
            Ok(()) => {}
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

/// With own relays the endpoint is at one of them: the chosen one (first in
/// the list). When the one in use has not been connected for
/// `FALLBACK_AFTER`, the next own relay that answers stands in; when none
/// does, the public relays help (if allowed), so devices still meet and find
/// a direct way; video stays off them as always. Every `FALLBACK_RECHECK`
/// the chosen relay is asked whether it answers again, and taken back then.
async fn fallback_loop(sh: Arc<Shared>) {
    use std::sync::atomic::Ordering::SeqCst;
    let mut down_since: Option<Instant> = None;
    let mut asked: Option<Instant> = None;
    let mut last: Option<Vec<RelayUrl>> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if sh.is_stopped() {
            return;
        }
        let (groups, public_ok, cur) = {
            let st = sh.st.lock().unwrap();
            (st.settings.relays.clone(), st.settings.public_relays, current_group(&st.settings, &st.using))
        };
        let Some(cur) = cur else {
            // No own relay: the public relays are the endpoint's own anyway (`apply_relays`).
            sh.fallback.store(false, SeqCst);
            (down_since, last) = (None, None);
            continue;
        };
        if last.as_ref() != Some(&cur) {
            down_since = None;
            last = Some(cur.clone());
        }
        let chosen = groups[0].clone();
        let on = sh.fallback.load(SeqCst);
        if net::connected_to(&sh.ep(), &cur) || (on && !public_ok) {
            down_since = None;
            if on {
                public_out(&sh, if public_ok { "an own relay is connected again" } else { "public relays turned off" }).await;
            }
            if cur != chosen && asked.is_none_or(|t| t.elapsed() >= FALLBACK_RECHECK) {
                asked = Some(Instant::now());
                if first_answering(std::slice::from_ref(&chosen)).await.is_some() {
                    log!("the chosen relay {} answers again: using it", chosen[0]);
                    use_group(&sh, None).await;
                }
            }
            continue;
        }
        if on {
            if asked.is_none_or(|t| t.elapsed() >= FALLBACK_RECHECK) {
                asked = Some(Instant::now());
                if let Some(g) = first_answering(&groups).await {
                    use_group(&sh, Some(g)).await;
                    public_out(&sh, "an own relay answers again").await;
                    down_since = None;
                }
            }
            continue;
        }
        let since = *down_since.get_or_insert_with(Instant::now);
        let down = since.elapsed();
        if down < FALLBACK_AFTER {
            continue;
        }
        // Tried again after another `FALLBACK_AFTER` if nothing answers now.
        down_since = Some(Instant::now());
        let others: Vec<Vec<RelayUrl>> = groups.iter().filter(|g| **g != cur).cloned().collect();
        if let Some(g) = first_answering(&others).await {
            log!("{} not connected for {} s: {} stands in", cur[0], down.as_secs(), g[0]);
            use_group(&sh, Some(g)).await;
            asked = Some(Instant::now());
            continue;
        }
        if public_ok {
            log!("no own relay connected for {} s: the public relays help meanwhile", down.as_secs());
            for u in net::public_relays() {
                sh.ep().insert_relay(u.clone(), Arc::new(RelayConfig::from(u))).await;
            }
            sh.fallback.store(true, SeqCst);
            asked = Some(Instant::now());
            wake_all(&sh);
        }
    }
}

/// The first of `groups` (own relays) that answers its probe.
async fn first_answering(groups: &[Vec<RelayUrl>]) -> Option<Vec<RelayUrl>> {
    for g in groups {
        if rc_relay::client::ping(g, &rc_relay::tls::ca()).await.is_ok() {
            return Some(g.clone());
        }
    }
    None
}

/// The endpoint at the own relay `g` (`None`: the chosen one) from now on.
async fn use_group(sh: &Arc<Shared>, g: Option<Vec<RelayUrl>>) {
    let (before, after) = {
        let mut st = sh.st.lock().unwrap();
        let before = relays_of(&st.settings, &st.using);
        let chosen = st.settings.relays.first().cloned();
        st.using = g.filter(|g| Some(g) != chosen.as_ref());
        (before, relays_of(&st.settings, &st.using))
    };
    if before != after {
        apply_relays(&sh.ep(), &before, &after).await;
        wake_all(sh);
        sh.changed();
    }
}

async fn public_out(sh: &Arc<Shared>, why: &str) {
    log!("{why}: the public relays leave the map");
    for u in net::public_relays() {
        sh.ep().remove_relay(&u).await;
    }
    sh.fallback.store(false, std::sync::atomic::Ordering::SeqCst);
    wake_all(sh);
}

/// Every device task says hello again (the home relay changed) or, if it is
/// waiting to retry, tries now.
fn wake_all(sh: &Shared) {
    for p in sh.st.lock().unwrap().peers.values() {
        p.wake.notify_one();
    }
}

/// `p`'s self-hosted relay (its home as last heard), if it is at one.
fn their_relay(p: &Peer, d: Option<&store::Device>) -> Option<Vec<RelayUrl>> {
    let home = if p.home.is_empty() { d.map(|d| d.relays.clone()).unwrap_or_default() } else { p.home.clone() };
    let own: Vec<RelayUrl> = home.into_iter().filter(|u| net::public_index(u).is_none()).collect();
    (!own.is_empty()).then_some(own)
}

/// Without an own relay in use (none, or none answers), this computer is
/// reachable only at public relays, which carry no video. So while a device
/// at a self-hosted relay is online, this computer stays at that relay too
/// (`target-plan` 5.2), without making it its home or telling others of it:
/// the relay only forwards between the two.
///
/// iroh keeps a relay that is not its home only while it sends through it,
/// so a meet connection with a path through the relay holds it there (each
/// path is pinged every few seconds). The device's own connection usually
/// has one; when it has none (the relay restarted, or the device moved to
/// another relay), a keeper opens one: a second meet connection that says no
/// hello. Tried again later and later while it does not get through.
async fn keep_loop(sh: Arc<Shared>) {
    loop {
        tokio::time::sleep(KEEP_EVERY).await;
        if sh.is_stopped() {
            return;
        }
        let ep = sh.ep();
        let mut dial = Vec::new();
        {
            let mut guard = sh.st.lock().unwrap();
            let St { settings, store, peers, .. } = &mut *guard;
            let at_own = net::home_relay(&ep).is_some_and(|h| group_of(settings, &h).is_some());
            for (id, p) in peers.iter_mut() {
                let want = {
                    let listed_or_code = store.get(id).is_some() || p.met == Some(Met::Code);
                    let main = p.conn.as_ref().filter(|c| c.close_reason().is_none());
                    match main {
                        Some(c) if !at_own && listed_or_code => their_relay(p, store.get(id)).filter(|r| {
                            !net::through_relay(c, r) && p.since.is_some_and(|t| t.elapsed() >= KEEP_OPEN)
                        }),
                        _ => None,
                    }
                };
                if let (Some(r), Some(k)) = (&want, &mut p.keeper)
                    && k.relay == *r
                    && k.conn.close_reason().is_none()
                {
                    if net::through_relay(&k.conn, r) {
                        if !k.held {
                            k.held = true;
                            p.keep_wait = KEEP_WAIT;
                            log!("staying at the relay of {} ({}) over a second connection", id.fmt_short(), r[0]);
                        }
                        continue;
                    }
                    if k.since.elapsed() < KEEP_OPEN {
                        continue;
                    }
                }
                if let Some(k) = p.keeper.take() {
                    k.conn.close(0u32.into(), b"bye");
                    match &want {
                        Some(_) if k.held => log!("the second connection to {} lost its path through {}", id.fmt_short(), k.relay[0]),
                        Some(_) => log!("the second connection to {} got no path through {} within {} s", id.fmt_short(), k.relay[0], KEEP_OPEN.as_secs()),
                        None => {}
                    }
                }
                if let Some(r) = want
                    && p.keep_after.is_none_or(|t| Instant::now() >= t)
                {
                    p.keep_after = Some(Instant::now() + p.keep_wait);
                    p.keep_wait = (p.keep_wait * 2).min(KEEP_WAIT_MAX);
                    dial.push((*id, r));
                }
            }
        }
        for (id, relay) in dial {
            tokio::spawn(keep_dial(sh.clone(), ep.clone(), id, relay));
        }
    }
}

/// Opens a keeper to `id` over `relay` (see `keep_loop`).
async fn keep_dial(sh: Arc<Shared>, ep: Endpoint, id: EndpointId, relay: Vec<RelayUrl>) {
    let target = net::dial_addr(id, &[], &relay);
    let r = tokio::time::timeout(CONNECT_WAIT, ep.connect(target, meet::ALPN)).await;
    let mut st = sh.st.lock().unwrap();
    match r {
        Ok(Ok(conn)) => match st.peers.get_mut(&id) {
            Some(p) if p.keeper.is_none() => p.keeper = Some(Keeper { conn, relay, since: Instant::now(), held: false }),
            _ => conn.close(0u32.into(), b"bye"),
        },
        Ok(Err(e)) => log!("second connection to {} over {}: {e:#}", id.fmt_short(), relay[0]),
        Err(_) => log!("second connection to {} over {}: no answer within {} s", id.fmt_short(), relay[0], CONNECT_WAIT.as_secs()),
    }
}

fn on_addr_change(sh: &Shared, ips_changed: bool) {
    {
        let mut st = sh.st.lock().unwrap();
        for p in st.peers.values_mut() {
            if ips_changed {
                p.lan = None;
            }
            // Another network: a relay that did not answer may answer now.
            p.keep_after = None;
            p.keep_wait = KEEP_WAIT;
        }
    }
    wake_all(sh);
    sh.changed();
}

/// When this computer's addresses or home relay change (another network,
/// a new IP, the relay reconnected), every device is tried again at once
/// instead of at its next round, and hears where this computer is now.
async fn network_loop(sh: Arc<Shared>, ep: Endpoint) {
    let mut w = ep.watch_addr();
    let mut last = w.get();
    loop {
        tokio::select! {
            r = w.updated() => if r.is_err() { return },
            _ = ep.closed() => return,
        }
        if sh.is_stopped() {
            return;
        }
        // A change rarely comes alone: let the rest arrive.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = w.get();
        if now == last {
            continue;
        }
        let ips = |a: &iroh::EndpointAddr| a.ip_addrs().copied().collect::<BTreeSet<SocketAddr>>();
        let relays = |a: &iroh::EndpointAddr| a.relay_urls().cloned().collect::<BTreeSet<RelayUrl>>();
        let ips_changed = ips(&now) != ips(&last);
        if ips_changed {
            log!("addresses now: {}", ips(&now).iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "));
        }
        if relays(&now) != relays(&last) {
            log!("home relay now: {}", relays(&now).iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", "));
        }
        last = now;
        on_addr_change(&sh, ips_changed);
    }
}

/// What runs on each endpoint until it closes: taking connections,
/// following its addresses, answering LAN discovery with its port.
fn serve_endpoint(sh: &Arc<Shared>, ep: &Endpoint) {
    tokio::spawn(accept_loop(sh.clone(), ep.clone()));
    tokio::spawn(network_loop(sh.clone(), ep.clone()));
    if sh.opts.lan {
        let s2 = sh.clone();
        discovery::respond(
            ep.clone(),
            Arc::new(move |asker| {
                let st = s2.st.lock().unwrap();
                let listed = asker.is_some_and(|a| st.store.get(&a).is_some());
                let allowed = if st.settings.lan_visible {
                    s2.lobby_open(&st) || listed
                } else {
                    listed
                };
                allowed.then(|| st.settings.shown_name())
            }),
        );
    }
}

/// An endpoint takes part in IPv6 or not from the start (`net::ipv6_detour`),
/// and a laptop started at home behind a proxy router keeps no IPv6 on
/// mobile data, where it is often the only way through (both sides behind
/// a carrier's NAT). So when the answer changes, a new endpoint, once no
/// session is hosted here: the cards go offline for a moment and come back.
async fn v6_loop(sh: Arc<Shared>) {
    use std::sync::atomic::Ordering::SeqCst;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if sh.is_stopped() || sh.leaving.load(SeqCst) {
            return;
        }
        let now = net::ipv6_detour().is_some();
        if now == sh.detoured.load(SeqCst) {
            continue;
        }
        let busy = {
            let st = sh.st.lock().unwrap();
            st.hosting.is_some() || st.asking.is_some() || host::session_count() > 0
        };
        if busy {
            continue;
        }
        log!("IPv6 {}: a new endpoint", if now { "now goes through a gateway in between: no longer used" } else { "of this computer's own now: used from now on" });
        if let Err(e) = renew(&sh, now).await {
            log!("new endpoint: {e:#}; trying again");
        }
    }
}

async fn renew(sh: &Arc<Shared>, detoured: bool) -> Result<()> {
    use std::sync::atomic::Ordering::SeqCst;
    let relays = {
        let st = sh.st.lock().unwrap();
        relays_of(&st.settings, &st.using)
    };
    // First, so the relay never sees two endpoints with this id. Meet
    // connections end with it; both sides dial again (this one at the
    // addresses it last reached the others at, `Peer::reached_at`).
    let old = sh.ep();
    let _ = tokio::time::timeout(Duration::from_millis(1500), old.close()).await;
    let ep = net::endpoint(sh.secret.clone(), &relays, true).await?;
    if sh.fallback.load(SeqCst) {
        for u in net::public_relays() {
            ep.insert_relay(u.clone(), Arc::new(RelayConfig::from(u))).await;
        }
    }
    for a in ep.bound_sockets() {
        log!("node: listening on {a}");
    }
    *sh.ep.write().unwrap() = ep.clone();
    sh.detoured.store(detoured, SeqCst);
    serve_endpoint(sh, &ep);
    wake_all(sh);
    sh.changed();
    Ok(())
}

/// The pipe the lobby uses to reach the node in the service's helper.
pub const PIPE: &str = r"\\.\pipe\remote-control-node";

/// The pipe of a node without the service (in the lobby or `host`) in
/// Windows session `session`, for `remote-control ctl`.
fn user_pipe(session: u32) -> String {
    format!(r"\\.\pipe\remote-control-node-s{session}")
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

/// Serves `node.request` on its pipe: the service's to the user signed in
/// on the helper's session, else this session's to this user only.
pub fn serve_pipe(node: &Node) {
    let n = node.clone();
    let (name, access) = if node.sh.opts.service {
        (PIPE.to_string(), crate::pipe::Access::SessionUser)
    } else {
        (user_pipe(my_session()), crate::pipe::Access::Owner)
    };
    let r = std::thread::Builder::new().name("node-pipe".into()).spawn(move || {
        let r = crate::pipe::serve(&name, access, move |req| n.request(req));
        log!("node pipe {name}: {r:?}");
    });
    if let Err(e) = r {
        log!("node pipe thread: {e}");
    }
}

/// One request to the running node: the service's, else the one in this
/// session (or, from a session without a desktop such as ssh, the console's).
pub fn call(req: &str) -> Result<String> {
    let wait = Duration::from_secs(2);
    if let Ok(a) = crate::pipe::call(PIPE, req, wait) {
        return Ok(a);
    }
    let mut sessions = vec![my_session()];
    let console = unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
    if !sessions.contains(&console) {
        sessions.push(console);
    }
    let mut last = anyhow!("no remote-control is running");
    for s in sessions {
        match crate::pipe::call(&user_pipe(s), req, wait) {
            Ok(a) => return Ok(a),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The node of `remote-control host` and of the service's helper: no window.
pub async fn run_headless(opts: Options) -> Result<()> {
    let node = Node::start(opts).await.context("start the node")?;
    serve_pipe(&node);
    let code = crate::goaway::wait().await;
    node.goodbye(code).await;
    crate::goaway::done();
    Ok(())
}

#[cfg(test)]
mod live {
    use super::*;

    /// Devices whose own calls never get through (`peer_loop`).
    pub static NO_DIAL: std::sync::Mutex<BTreeSet<EndpointId>> = std::sync::Mutex::new(BTreeSet::new());

    /// Only B can reach A (A's calls never get through, as from behind a
    /// proxy): A still shows B online, over B's connection, until B goes.
    #[test]
    fn one_way_reach_shows_on_both_sides() {
        let dir = std::env::temp_dir().join(format!("rc-oneway-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        NO_DIAL.lock().unwrap().insert(na.sh.me);
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let online = |n: &Node, id: EndpointId| {
            ask(n, "snapshot").lines().any(|l| l.starts_with(&format!("dev\t{id}\t")) && l.contains("\tonline\t"))
        };
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(20), "{what}");
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        wait("a code with addresses", &|| ConnCode::find(&ask(&na, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        let code = ask(&na, "code");
        ask(&nb, &format!("paste\t{code}"));
        let (ida, idb) = (na.sh.me, nb.sh.me);
        wait("B sees A online", &|| online(&nb, ida));
        wait("A sees B online over B's connection", &|| online(&na, idb));
        // Past A's failed calls (CONNECT_WAIT): they leave the card alone.
        std::thread::sleep(CONNECT_WAIT + Duration::from_secs(1));
        assert!(online(&na, idb), "A's failed call took B's card offline");
        rt.block_on(nb.goodbye(0));
        wait("A sees B gone", &|| !online(&na, idb));
        NO_DIAL.lock().unwrap().remove(&ida);
        rt.block_on(na.goodbye(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Over the internet: a stranger with a connection code meets the code's
    /// owner through the owner's relay (the code without addresses, so only
    /// that relay can carry the first packets), and does not make that relay
    /// its home. `own`: the stranger's own relay, if it has one (an address
    /// nothing answers at does). RC_TEST_RELAY is a relay (its address) that
    /// the key in RC_TEST_OWNER_KEY owns. Run with `--ignored`.
    fn stranger_meets_through_the_owners_relay(name: &str, own: Option<&str>) {
        let relay = std::env::var("RC_TEST_RELAY").expect("RC_TEST_RELAY");
        let owner = std::env::var("RC_TEST_OWNER_KEY").expect("RC_TEST_OWNER_KEY");
        let dir = std::env::temp_dir().join(format!("rc-{name}-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::copy(&owner, a.join("device.key")).unwrap();
        std::fs::write(a.join("settings.txt"), format!("relay={relay}\npublic_relays=1\n")).unwrap();
        let b_own = own.map(|u| format!("relay={u}\n")).unwrap_or_default();
        std::fs::write(b.join("settings.txt"), format!("{b_own}public_relays=1\n")).unwrap();
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let t = Instant::now();
        while !ask(&na, "snapshot").contains("\thome\n") {
            assert!(t.elapsed() < Duration::from_secs(20), "own relay not reached");
            std::thread::sleep(Duration::from_millis(200));
        }
        let (c, _) = ConnCode::find(&ask(&na, "code")).unwrap();
        assert_eq!(c.device, na.sh.me, "the code names its owner");
        let Find::Relay(at) = &c.find else { panic!("{:?}", c.find) };
        let theirs = at.clone();
        let code = ConnCode { addrs: Vec::new(), ..c }.encode();
        ask(&nb, &format!("paste\t{code}"));
        let online = |n: &Node, id: EndpointId, what: &str| {
            let t = Instant::now();
            loop {
                let s = ask(n, "snapshot");
                let row = s.lines().find(|l| l.starts_with(&format!("dev\t{id}\t")));
                if let Some(r) = row.filter(|r| r.contains("\tonline\t")) {
                    println!("{what}: {r}");
                    break;
                }
                assert!(t.elapsed() < Duration::from_secs(30), "{what}: not reached:\n{s}");
                std::thread::sleep(Duration::from_millis(300));
            }
        };
        online(&nb, na.sh.me, "the stranger sees the owner");
        online(&na, nb.sh.me, "the owner sees the stranger");
        let home = net::home_relay(&nb.sh.ep());
        println!("the stranger's home relay: {home:?}");
        assert!(home.is_none_or(|h| !theirs.contains(&h)), "the owner's relay became the stranger's home");
        let told = na.sh.st.lock().unwrap().peers[&nb.sh.me].home.clone();
        assert!(!told.iter().any(|h| theirs.contains(h)), "the stranger says it is at the owner's relay: {told:?}");
        rt.block_on(async {
            na.goodbye(0).await;
            nb.goodbye(0).await;
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn a_stranger_meets_through_the_owners_relay() {
        stranger_meets_through_the_owners_relay("invite", None);
    }

    #[test]
    #[ignore]
    fn a_stranger_with_an_own_relay_meets_through_the_owners_relay() {
        stranger_meets_through_the_owners_relay("invite-own", Some("https://127.0.0.1:9/"));
    }

    /// Over the internet: the owner's relay forwards for a stranger met with
    /// a code, and stops once the owner forgets it (its lobby closed). The
    /// owner: RC_TEST_OWNER_KEY at the relay RC_TEST_RELAY, or a new key that
    /// claims RC_TEST_RELAY_CODE (`ctl relay-add <relay>` on an owner). Run
    /// with `--ignored`.
    #[test]
    #[ignore]
    fn the_relay_stops_forwarding_for_a_forgotten_stranger() {
        let dir = std::env::temp_dir().join(format!("rc-guests-{}", std::process::id()));
        let (a, b, probe) = (dir.join("a"), dir.join("b"), dir.join("probe"));
        for d in [&a, &b, &probe] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let owner = std::env::var("RC_TEST_OWNER_KEY").ok();
        if let Some(k) = &owner {
            std::fs::copy(k, a.join("device.key")).unwrap();
            let relay = std::env::var("RC_TEST_RELAY").expect("RC_TEST_RELAY");
            std::fs::write(a.join("settings.txt"), format!("relay={relay}\npublic_relays=0\nlan_visible=0\n")).unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(30), "not in 30 s: {what}");
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        if owner.is_none() {
            let code = std::env::var("RC_TEST_RELAY_CODE").expect("RC_TEST_RELAY_CODE or RC_TEST_OWNER_KEY");
            ask(&na, &format!("relay-claim\t{code}"));
        }
        wait("the owner at its relay", &|| {
            let s = ask(&na, "snapshot");
            println!("{}", s.lines().filter(|l| l.starts_with("relay")).collect::<Vec<_>>().join(" | "));
            s.contains("\thome\n")
        });
        let (c, _) = ConnCode::find(&ask(&na, "code")).unwrap();
        let Find::Relay(relay) = c.find.clone() else { panic!("{:?}", c.find) };
        let ida = na.sh.me;
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        ask(&nb, &format!("paste\t{}", ConnCode { addrs: Vec::new(), ..c }.encode()));
        let online = |n: &Node, id: EndpointId| {
            ask(n, "snapshot").lines().any(|l| l.starts_with(&format!("dev\t{id}\t")) && l.contains("\tonline\t"))
        };
        wait("the stranger meets the owner", &|| online(&nb, ida));
        rt.block_on(nb.goodbye(0));
        // A fresh endpoint with the stranger's key (and like it, no relay of
        // its own) knows no address of the owner: only the relay can carry
        // the handshake. A folder of its own each time: the key stays marked
        // online until its node is dropped.
        let forwards = |name: &str| {
            let d = probe.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::copy(b.join("device.key"), d.join("device.key")).unwrap();
            std::fs::copy(probe.join("settings.txt"), d.join("settings.txt")).unwrap();
            let n = rt.block_on(Node::start(opts(&d))).unwrap();
            let target = net::dial_addr(ida, &[], &relay);
            let ep = n.sh.ep();
            let r = rt.block_on(async move { tokio::time::timeout(CONNECT_WAIT, ep.connect(target, meet::ALPN)).await });
            let ok = matches!(r, Ok(Ok(_)));
            println!("relay-only dial: {}", match r {
                Ok(Ok(c)) => format!("connected via {:?}", net::selected_path(&c).0),
                Ok(Err(e)) => format!("{e:#}"),
                Err(_) => "timed out".into(),
            });
            rt.block_on(n.goodbye(0));
            ok
        };
        assert!(forwards("before"), "the relay should forward for a guest");
        lobby_closed(&mut na.sh.st.lock().unwrap());
        na.sh.changed();
        std::thread::sleep(Duration::from_secs(5));
        assert!(!forwards("after"), "the relay still forwards for a forgotten stranger");
        println!("owner key (remove from the relay's owners.txt): {ida}");
        rt.block_on(na.goodbye(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On this computer, no relay: B meets A with A's code, then A forgets
    /// B in `forget`. Both drop the other's card for good: B must not keep
    /// showing A online over the meet connection it opened itself.
    /// `one_way`: A's own calls never get through. Returns how long B took
    /// to drop A.
    fn meet_then_forget(name: &str, one_way: bool, forget: impl Fn(&Node, EndpointId)) -> Duration {
        let dir = std::env::temp_dir().join(format!("rc-{name}-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            // Not visible on the LAN: on one computer the LAN rule would let B back in.
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        if one_way {
            NO_DIAL.lock().unwrap().insert(na.sh.me);
        }
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let row = |n: &Node, id: EndpointId| {
            ask(n, "snapshot").lines().find(|l| l.starts_with(&format!("dev\t{id}\t"))).map(str::to_string)
        };
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(20), "{what}");
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        wait("a code with addresses", &|| ConnCode::find(&ask(&na, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        let code = ask(&na, "code");
        ask(&nb, &format!("paste\t{code}"));
        let (ida, idb) = (na.sh.me, nb.sh.me);
        wait("B sees A online", &|| row(&nb, ida).is_some_and(|r| r.contains("\tonline\t")));
        wait("A sees B online", &|| row(&na, idb).is_some_and(|r| r.contains("\tonline\t")));
        forget(&na, idb);
        let t = Instant::now();
        let gone = na.sh.is_stopped();
        wait("A drops B", &|| gone || row(&na, idb).is_none());
        wait("B drops A", &|| row(&nb, ida).is_none());
        let took = t.elapsed();
        std::thread::sleep(Duration::from_secs(3));
        assert!((gone || row(&na, idb).is_none()) && row(&nb, ida).is_none(), "a card came back");
        NO_DIAL.lock().unwrap().remove(&ida);
        rt.block_on(async {
            na.goodbye(0).await;
            nb.goodbye(0).await;
        });
        let _ = std::fs::remove_dir_all(&dir);
        took
    }

    /// As when A's lobby closes in service mode.
    #[test]
    fn a_forgotten_stranger_drops_the_card() {
        meet_then_forget("forget", false, |n, _| lobby_closed(&mut n.sh.st.lock().unwrap()));
    }

    /// A closes its lobby (no service): B drops the card at once, not when
    /// its calls stop getting through.
    #[test]
    fn a_stranger_that_goes_away_drops_the_card() {
        let took = meet_then_forget("leave", false, |n, _| n.sh.rt.block_on(n.stop()));
        assert!(took < Duration::from_secs(2), "the other side took {took:?}");
    }

    /// IPv6 came or went on A (`v6_loop`): with a new endpoint, both sides
    /// show each other again, and B did not take it for A going away.
    #[test]
    fn a_new_endpoint_keeps_the_cards() {
        let dir = std::env::temp_dir().join(format!("rc-renew-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let online = |n: &Node, id: EndpointId| {
            ask(n, "snapshot").lines().any(|l| l.starts_with(&format!("dev\t{id}\t")) && l.contains("\tonline\t"))
        };
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(20), "{what}");
                std::thread::sleep(Duration::from_millis(200));
            }
            t.elapsed()
        };
        wait("a code with addresses", &|| ConnCode::find(&ask(&na, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        ask(&nb, &format!("paste\t{}", ask(&na, "code")));
        let (ida, idb) = (na.sh.me, nb.sh.me);
        wait("B sees A", &|| online(&nb, ida));
        wait("A sees B", &|| online(&na, idb));
        let before = na.sh.ep().bound_sockets();
        let detoured = na.sh.detoured.load(std::sync::atomic::Ordering::SeqCst);
        rt.block_on(renew(&na.sh, detoured)).unwrap();
        assert_ne!(na.sh.ep().bound_sockets(), before, "still the old endpoint");
        let took = wait("A sees B again", &|| online(&na, idb));
        wait("B sees A again", &|| online(&nb, ida));
        println!("back after {took:?}");
        rt.block_on(async {
            na.goodbye(0).await;
            nb.goodbye(0).await;
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn delete(n: &Node, id: EndpointId) {
        let r = n.request(&format!("remove\t{id}"));
        assert!(r.starts_with("ok"), "{r}");
    }

    /// The delete button: it takes effect at once, so the other side
    /// drops the card within a moment.
    #[test]
    fn deleting_a_stranger_deletes_on_both_sides() {
        let took = meet_then_forget("delete", false, delete);
        assert!(took < Duration::from_secs(2), "the other side took {took:?}");
    }

    /// The same when only the other side's connection holds (A's calls
    /// don't get through): A closes it, B calls again and is refused.
    #[test]
    fn deleting_over_the_other_sides_connection() {
        let took = meet_then_forget("delete1", true, delete);
        assert!(took < Duration::from_secs(4), "the other side took {took:?}");
    }

    /// On this computer, no relay: B meets A with A's code and asks to
    /// remember each other. A refuses, B asks again, A agrees: both list the
    /// other. Then B asks a new stranger C, which goes away without an
    /// answer. `one_way`: A's own calls never get through, so A's answers
    /// reach B only in welcomes.
    fn remembering_takes_both_sides(name: &str, one_way: bool) {
        let dir = std::env::temp_dir().join(format!("rc-{name}-{}", std::process::id()));
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        let nc = rt.block_on(Node::start(opts(&c))).unwrap();
        let (ida, idb, idc) = (na.sh.me, nb.sh.me, nc.sh.me);
        if one_way {
            NO_DIAL.lock().unwrap().insert(ida);
        }
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        // (listed, ask) of `id`'s row on `n`.
        let row = |n: &Node, id: EndpointId| {
            let s = ask(n, "snapshot");
            let l = s.lines().find(|l| l.starts_with(&format!("dev\t{id}\t")))?.to_string();
            let f: Vec<&str> = l.split('\t').collect();
            Some((f[2] == "1", f[10].to_string()))
        };
        let notice = |n: &Node| ask(n, "snapshot").lines().find_map(|l| l.strip_prefix("notice\t").map(str::to_string)).unwrap_or_default();
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(30), "{what}");
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        let is = |n: &Node, id: EndpointId, listed: bool, a: &str| row(n, id) == Some((listed, a.to_string()));
        wait("a code with addresses", &|| ConnCode::find(&ask(&na, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        ask(&nb, &format!("paste\t{}", ask(&na, "code")));
        wait("B meets A", &|| is(&nb, ida, false, "-"));
        wait("A meets B", &|| is(&na, idb, false, "-"));

        assert_eq!(ask(&nb, &format!("remember\t{ida}")), "");
        assert!(is(&nb, ida, false, "out"));
        wait("A is asked", &|| is(&na, idb, false, "in"));
        ask(&na, &format!("refuse\t{idb}"));
        wait("B hears no", &|| is(&nb, ida, false, "-") && (notice(&nb).contains("没有同意") || notice(&nb).contains("declined")));
        assert!(is(&na, idb, false, "-"));

        ask(&nb, &format!("remember\t{ida}"));
        wait("A is asked again", &|| is(&na, idb, false, "in"));
        ask(&na, &format!("agree\t{idb}"));
        assert!(is(&na, idb, true, "-"), "A lists B at once");
        wait("B lists A", &|| is(&nb, ida, true, "-") && (notice(&nb).contains("同意了") || notice(&nb).contains("agreed")));
        std::thread::sleep(Duration::from_secs(3));
        assert!(is(&na, idb, true, "-") && is(&nb, ida, true, "-"), "a side changed its mind");
        assert!(!na.sh.st.lock().unwrap().store.get(&idb).unwrap().auto_accept, "trusted, not 我的电脑");

        wait("a code from C", &|| ConnCode::find(&ask(&nc, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        ask(&nb, &format!("paste\t{}", ask(&nc, "code")));
        wait("B meets C", &|| is(&nb, idc, false, "-"));
        ask(&nb, &format!("remember\t{idc}"));
        wait("C is asked", &|| is(&nc, idb, false, "in"));
        rt.block_on(nc.stop());
        wait("B hears nothing back", &|| notice(&nb).contains("没有回应") || notice(&nb).contains("did not respond"));
        assert!(row(&nb, idc).is_none_or(|(l, a)| !l && a == "-"));

        NO_DIAL.lock().unwrap().remove(&ida);
        rt.block_on(async {
            na.goodbye(0).await;
            nb.goodbye(0).await;
            nc.goodbye(0).await;
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remembering_asks_the_other_side() {
        remembering_takes_both_sides("remember", false);
    }

    #[test]
    fn remembering_over_the_other_sides_connection() {
        remembering_takes_both_sides("remember1", true);
    }

    /// Both click 记住 at the same moment: they remember each other.
    #[test]
    fn both_asking_remember_each_other() {
        let dir = std::env::temp_dir().join(format!("rc-both-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        let (ida, idb) = (na.sh.me, nb.sh.me);
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let listed = |n: &Node, id: EndpointId| n.sh.st.lock().unwrap().store.get(&id).is_some();
        let online = |n: &Node, id: EndpointId| {
            ask(n, "snapshot").lines().any(|l| l.starts_with(&format!("dev\t{id}\t")) && l.contains("\tonline\t"))
        };
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(20), "{what}");
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        wait("a code with addresses", &|| ConnCode::find(&ask(&na, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        ask(&nb, &format!("paste\t{}", ask(&na, "code")));
        wait("B sees A", &|| online(&nb, ida));
        wait("A sees B", &|| online(&na, idb));
        ask(&na, &format!("remember\t{idb}"));
        ask(&nb, &format!("remember\t{ida}"));
        wait("both remember", &|| listed(&na, idb) && listed(&nb, ida));
        std::thread::sleep(Duration::from_secs(3));
        assert!(listed(&na, idb) && listed(&nb, ida));
        rt.block_on(async {
            na.goodbye(0).await;
            nb.goodbye(0).await;
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    fn id(n: u8) -> EndpointId {
        SecretKey::from_bytes(&[n; 32]).public()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-handoff-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_hosting_peers_and_codes() {
        let dir = temp_dir("rt");
        let path = handoff_path(&dir.join("device.key"));
        let mut store = Store::empty(&store::devices_path(&dir.join("device.key")));
        store.add(id(10)).name = "listed".into();

        let mut peers = BTreeMap::new();
        // Listed device met with code should NOT be serialized as a temp peer
        let mut p_listed = Peer::new();
        p_listed.name = "listed".into();
        p_listed.met = Some(Met::Code);
        peers.insert(id(10), p_listed);

        // LAN-only peer should NOT be serialized
        let mut p_lan = Peer::new();
        p_lan.name = "lan".into();
        p_lan.met = Some(Met::Lan);
        peers.insert(id(11), p_lan);

        // Temporary Met::Code peer SHOULD be serialized
        let mut p_code = Peer::new();
        p_code.name = "书房\t电脑\n".into();
        p_code.met = Some(Met::Code);
        p_code.home = vec!["https://203.0.113.7:7843/?rc-cert=abc".parse().unwrap()];
        p_code.reached_at = Some("192.168.1.10:5000".parse().unwrap());
        p_code.code_addrs = vec!["192.168.1.10:5000".parse().unwrap(), "10.0.0.2:6000".parse().unwrap()];
        p_code.token = Some([0xab; 8]);
        p_code.invite = Some(vec!["https://203.0.113.8:7843/?rc-cert=def".parse().unwrap()]);
        peers.insert(id(12), p_code);

        let codes = vec![
            Issued {
                token: [1; 8],
                text: "rc1unused".into(),
                made: Instant::now().checked_sub(Duration::from_secs(120)).unwrap_or_else(Instant::now),
                expires: 2_000_000_000,
                by: None,
            },
            Issued {
                token: [2; 8],
                text: "rc1used".into(),
                made: Instant::now().checked_sub(Duration::from_secs(300)).unwrap_or_else(Instant::now),
                expires: 2_000_000_000,
                by: Some(id(12)),
            },
            // Expired unused code should NOT be serialized
            Issued {
                token: [3; 8],
                text: "rc1expired".into(),
                made: Instant::now().checked_sub(CODE_LIFETIME + Duration::from_secs(60)).unwrap_or_else(Instant::now),
                expires: 1_000,
                by: None,
            },
        ];

        let now = 1_700_000_000u64;
        write_handoff(&path, Some((id(12), "书房\t控制端")), &peers, &store, &codes, now).unwrap();
        assert!(path.exists());

        let loaded = load_handoff_at(&path, now + 10).expect("valid handoff");
        // Immediately deleted on read
        assert!(!path.exists());

        let (hid, hname, rem) = loaded.hosting.expect("hosting");
        assert_eq!(hid, id(12));
        assert_eq!(hname, "书房 控制端");
        assert_eq!(rem, HANDOFF_TTL_SECS - 10);

        assert_eq!(loaded.peers.len(), 1);
        let (pid, rp) = &loaded.peers[0];
        assert_eq!(*pid, id(12));
        assert_eq!(rp.name, "书房 电脑");
        assert_eq!(rp.met, Some(Met::Code));
        assert_eq!(rp.home, vec!["https://203.0.113.7:7843/?rc-cert=abc".parse::<RelayUrl>().unwrap()]);
        assert_eq!(
            rp.code_addrs,
            vec!["192.168.1.10:5000".parse::<SocketAddr>().unwrap(), "10.0.0.2:6000".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(rp.token, Some([0xab; 8]));
        assert_eq!(rp.invite, Some(vec!["https://203.0.113.8:7843/?rc-cert=def".parse::<RelayUrl>().unwrap()]));

        assert_eq!(loaded.codes.len(), 2);
        assert_eq!(loaded.codes[0].token, [1; 8]);
        assert_eq!(loaded.codes[0].text, "rc1unused");
        assert_eq!(loaded.codes[0].by, None);
        assert_eq!(loaded.codes[1].token, [2; 8]);
        assert_eq!(loaded.codes[1].by, Some(id(12)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_missing_and_malformed_handoff_are_ignored() {
        let dir = temp_dir("exp");
        let path = handoff_path(&dir.join("device.key"));
        assert!(load_handoff_at(&path, 1_700_000_000).is_none());

        let store = Store::empty(&store::devices_path(&dir.join("device.key")));
        let peers = BTreeMap::new();
        let now = 1_700_000_000u64;
        write_handoff(&path, Some((id(1), "host")), &peers, &store, &[], now).unwrap();
        // Past expires_unix -> ignored and deleted
        assert!(load_handoff_at(&path, now + HANDOFF_TTL_SECS).is_none());
        assert!(!path.exists());

        // Malformed file -> ignored and deleted
        std::fs::write(&path, "expires\tnot-a-number\n").unwrap();
        assert!(load_handoff_at(&path, now).is_none());
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lan_temp_peer_stays_strictly_on_lan_and_never_uses_relays() {
        let dir = temp_dir("lan_only");
        std::fs::write(dir.join("settings.txt"), "public_relays=1\nlan_visible=1\n").unwrap();
        let mut o = host::HostArgs::defaults().node_options(false);
        o.key = dir.join("device.key");
        o.lan = false;
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let node = rt.block_on(Node::start(o)).unwrap();

        let peer = id(42);
        {
            let mut st = node.sh.st.lock().unwrap();
            let mut p = Peer::new();
            p.met = Some(Met::Lan);
            p.lan = Some(("192.168.1.50:5555".parse().unwrap(), Instant::now()));
            p.reached_at = Some("203.0.113.9:4444".parse().unwrap());
            p.home = vec!["https://203.0.113.7:7843/?rc-cert=abc".parse().unwrap()];
            p.status = Status::Unreachable;
            st.peers.insert(peer, p);

            // While seen on the LAN: wanted, knows, and dials ONLY LAN addresses with no relays.
            assert!(node.sh.wanted(&st, &peer));
            assert!(node.sh.knows(&st, &peer));
            let (addrs, relays) = node.sh.dial_plan(&st, &peer, 1);
            assert_eq!(addrs, vec!["192.168.1.50:5555".parse::<SocketAddr>().unwrap()]);
            assert!(relays.is_empty());

            // Once no longer seen on the LAN (and no live LAN connection): disappears even if
            // status was Unreachable or Online via public relay.
            st.peers.get_mut(&peer).unwrap().lan = None;
            assert!(!node.sh.wanted(&st, &peer));
            assert!(!node.sh.knows(&st, &peer));
            st.peers.get_mut(&peer).unwrap().status = Status::Online { rtt_ms: 500.0, path: PathKind::Public };
            assert!(!node.sh.wanted(&st, &peer));
            assert!(!node.sh.knows(&st, &peer));
        }

        // Only unlisted Met::Lan peers use the faster LAN probe cadence.
        {
            let mut st = node.sh.st.lock().unwrap();
            assert!(is_lan_temp(&st, &peer));

            let code_peer = id(43);
            let mut pc = Peer::new();
            pc.met = Some(Met::Code);
            st.peers.insert(code_peer, pc);
            assert!(!is_lan_temp(&st, &code_peer));

            let listed_lan = id(44);
            st.store.add(listed_lan).name = "listed".into();
            let mut pl = Peer::new();
            pl.met = Some(Met::Lan);
            pl.lan = Some(("192.168.1.51:5555".parse().unwrap(), Instant::now()));
            st.peers.insert(listed_lan, pl);
            assert!(!is_lan_temp(&st, &listed_lan));

            st.peers.get_mut(&peer).unwrap().lan = Some(("192.168.1.50:5555".parse().unwrap(), Instant::now()));
        }

        // Relay-only address change keeps `lan`; IP change clears `lan` for all peers.
        on_addr_change(&node.sh, false);
        assert!(node.sh.st.lock().unwrap().peers[&peer].lan.is_some());
        on_addr_change(&node.sh, true);
        {
            let st = node.sh.st.lock().unwrap();
            assert!(st.peers[&peer].lan.is_none());
            assert!(st.peers[&id(44)].lan.is_none());
            assert!(!node.sh.wanted(&st, &peer));
        }

        rt.block_on(node.goodbye(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression test: pasting a fresh connection code while an earlier delete
    /// (`forgotten`) is still pending must clear `forgotten`, drop any stale
    /// connection, and redeem the new code on the first paste so both sides
    /// show each other online without dropping.
    #[test]
    fn paste_after_pending_forgotten_succeeds_first_try() {
        let dir = temp_dir("paste_after_forget");
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.txt"), "public_relays=0\nlan_visible=0\n").unwrap();
        }
        let opts = |d: &std::path::Path| {
            let mut o = host::HostArgs::defaults().node_options(false);
            o.key = d.join("device.key");
            o.lan = false;
            o
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let na = rt.block_on(Node::start(opts(&a))).unwrap();
        let nb = rt.block_on(Node::start(opts(&b))).unwrap();
        let (ida, idb) = (na.sh.me, nb.sh.me);
        let ask = |n: &Node, req: &str| {
            let r = n.request(req);
            r.strip_prefix("ok\t").unwrap_or_else(|| panic!("{req}: {r}")).to_string()
        };
        let row = |n: &Node, id: EndpointId| {
            ask(n, "snapshot").lines().find(|l| l.starts_with(&format!("dev\t{id}\t"))).map(str::to_string)
        };
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !ok() {
                assert!(t.elapsed() < Duration::from_secs(20), "{what}");
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        // Simulate an undelivered prior delete of A left in B's store (and A
        // does not know B), plus an in-flight dial on B.
        {
            let mut st = nb.sh.st.lock().unwrap();
            st.store.add(ida).name = "A".into();
            st.store.forget(&ida);
            assert!(st.store.is_forgotten(&ida));
        }
        live::NO_DIAL.lock().unwrap().insert(idb);
        {
            let _g = rt.enter();
            let mut st = nb.sh.st.lock().unwrap();
            nb.sh.ensure(&mut st, ida);
        }
        std::thread::sleep(Duration::from_millis(150));

        // Now unblock B and paste A's fresh connection code once.
        live::NO_DIAL.lock().unwrap().remove(&idb);
        wait("A code with addresses", &|| ConnCode::find(&ask(&na, "code")).is_some_and(|(c, _)| !c.addrs.is_empty()));
        let code = ask(&na, "code");
        ask(&nb, &format!("paste\t{code}"));

        // `forgotten` is cleared immediately, wish is not EVENT_FORGET, and
        // both sides come online on the very first paste and stay online.
        {
            let st = nb.sh.st.lock().unwrap();
            assert!(!st.store.is_forgotten(&ida));
            assert_eq!(nb.sh.wish(&st, &ida), meet::EVENT_NONE);
            assert_eq!(st.peers[&ida].redial, 1);
        }
        wait("B sees A online", &|| row(&nb, ida).is_some_and(|r| r.contains("\tonline\t")));
        wait("A sees B online", &|| row(&na, idb).is_some_and(|r| r.contains("\tonline\t")));
        std::thread::sleep(Duration::from_secs(2));
        assert!(row(&nb, ida).is_some_and(|r| r.contains("\tonline\t")), "B dropped A");
        assert!(row(&na, idb).is_some_and(|r| r.contains("\tonline\t")), "A dropped B");

        rt.block_on(async {
            na.goodbye(0).await;
            nb.goodbye(0).await;
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
