//! iroh endpoint setup shared by the lobby's core, the host and the viewer.

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr,
    endpoint::{
        Connection, IdleTimeout, QuicTransportConfig, presets,
        transports::{Addr, PathSelection, PathSelectionContext, PathSelectionData, PathSelector},
    },
};

use crate::log;
use crate::proto::ALPN;

/// The free public relays run by n0 (iroh's defaults), by the number a
/// connection code carries. They only help two devices meet and find a
/// direct way; video and input never go through them (see `host`).
pub const PUBLIC_RELAYS: [&str; 4] = [
    "https://use1-1.relay.n0.iroh.link./",
    "https://usw1-1.relay.n0.iroh.link./",
    "https://euc1-1.relay.n0.iroh.link./",
    "https://aps1-1.relay.n0.iroh.link./",
];

pub fn public_relays() -> Vec<RelayUrl> {
    PUBLIC_RELAYS.iter().map(|u| u.parse().expect("valid relay URL")).collect()
}

/// The number of a public relay, if `url` is one.
pub fn public_index(url: &RelayUrl) -> Option<u8> {
    let host = url.host_str()?.trim_end_matches('.');
    PUBLIC_RELAYS.iter().position(|p| p.contains(&format!("//{host}."))).map(|i| i as u8)
}

pub fn public_relay(n: u8) -> Option<RelayUrl> {
    PUBLIC_RELAYS.get(n as usize).map(|u| u.parse().expect("valid relay URL"))
}

/// Outgoing datagram queue. Large enough for a key frame many times the size
/// of a normal frame; the frame budget keeps it short the rest of the time.
pub const SEND_BUFFER: usize = 4 * 1024 * 1024;

/// A peer that sends nothing for this long is gone: a host killed or cut
/// off sends no close, and quinn's default of 30 s left the viewer showing a
/// frozen picture that long. It also bounds the handshake, and a relay can
/// stall for 2 s or more, so 10 s; the viewer notices a silent host sooner
/// (`view::HOST_SILENT`). Both ends use the shorter of their two values.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(1);

/// Application close codes on the desktop connection (0 is a normal end).
/// The host has not met the viewer (no connection code, not on its LAN, not
/// in its device list).
pub const CLOSE_NOT_MET: u32 = 1;
pub const CLOSE_BUSY: u32 = 2;
/// The host's computer is restarting, shutting down or signing out.
pub const CLOSE_SHUTDOWN: u32 = 3;
/// The host program was closed or stopped (the service, the lobby, Ctrl+C).
pub const CLOSE_STOPPED: u32 = 4;
/// The host's user said no (now or earlier in this run).
pub const CLOSE_DENIED: u32 = 5;
/// Nobody answered the host's confirmation within 30 s.
pub const CLOSE_NO_ANSWER: u32 = 6;
/// The host is locked or at the login screen and the viewer is not one of its own computers.
pub const CLOSE_LOCKED: u32 = 7;
/// Only a public relay was left between the two for too long.
pub const CLOSE_NO_DIRECT: u32 = 8;
/// The host's user clicked "disconnect".
pub const CLOSE_DISCONNECTED: u32 = 9;

/// BBR (phase 0: far less queueing than Cubic on WiFi) kept from collapsing
/// on a still screen (see `cc`), and a datagram queue that never drops a key frame.
pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .congestion_controller_factory(crate::cc::factory())
        .datagram_send_buffer_size(SEND_BUFFER)
        .max_idle_timeout(Some(IdleTimeout::try_from(IDLE_TIMEOUT).expect("a few seconds fit")))
        .keep_alive_interval(KEEP_ALIVE)
        .build()
}

/// Which relays an endpoint keeps a connection to (its home is the fastest).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Relays {
    /// This computer's own relays; when non-empty, home instead of the public ones.
    pub own: Vec<RelayUrl>,
    /// Without an own relay: the public relays (else none at all).
    pub public: bool,
    /// The other side's self-hosted relays (a session window's peer). They
    /// let through only devices on their roster, so the session key must
    /// show its device proof there too.
    pub theirs: Vec<RelayUrl>,
    /// Sent to every self-hosted relay: a device proof, for a session key (see `ident`).
    pub token: Option<String>,
}

impl Relays {
    pub fn map(&self) -> Option<RelayMap> {
        rc_relay::tls::learn_all(self.own.iter().chain(&self.theirs));
        let config = |u: &RelayUrl| {
            let c = RelayConfig::from(u.clone());
            match &self.token {
                Some(t) => c.with_auth_token(t.clone()),
                None => c,
            }
        };
        let mut v: Vec<RelayConfig> = Vec::new();
        if !self.own.is_empty() {
            v.extend(self.own.iter().map(config));
        } else if self.public {
            v.extend(public_relays().into_iter().map(RelayConfig::from));
        }
        for u in &self.theirs {
            if public_index(u).is_none() && !self.own.contains(u) {
                v.push(config(u));
            }
        }
        (!v.is_empty()).then(|| RelayMap::from_iter(v))
    }
}

/// Each 1 % of packets lost on a path counts as 10 % more round trip.
const LOSS_WEIGHT: f64 = 10.0;
/// Fewer packets than this say nothing about a path's loss yet.
const LOSS_SAMPLE: u64 = 50;
/// A path counts as clearly better only with a score lower by this share
/// of the other's, and by at least `SWITCH_MIN`, so the choice does not flap.
const SWITCH_SHARE: f64 = 0.10;
const SWITCH_MIN: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    Direct,
    OwnRelay,
    PublicRelay,
}

/// One open path as the choice sees it.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    way: Way,
    /// Round trip with loss counted in, in seconds: lower is better.
    score: f64,
    current: bool,
}

/// Whether `a` is clearly better than `b`.
fn clearly_better(a: f64, b: f64) -> bool {
    a + (b * SWITCH_SHARE).max(SWITCH_MIN.as_secs_f64()) < b
}

/// The path to carry the data (`target-plan` 1, 5.2): the best score among
/// direct paths and self-hosted relays, a direct one when about as good (it
/// costs the relay nothing), the current one unless another is clearly
/// better. A public relay only while nothing else is open: it helps two
/// devices meet and never carries a session (see `host`).
fn choose(c: &[Candidate]) -> Option<usize> {
    let pool: Vec<usize> = match (0..c.len()).filter(|&i| c[i].way != Way::PublicRelay).collect::<Vec<_>>() {
        v if v.is_empty() => (0..c.len()).collect(),
        v => v,
    };
    let lowest = |ix: &mut dyn Iterator<Item = usize>| ix.min_by(|&a, &b| c[a].score.total_cmp(&c[b].score));
    let mut pick = lowest(&mut pool.iter().copied())?;
    if c[pick].way != Way::Direct
        && let Some(d) = lowest(&mut pool.iter().copied().filter(|&i| c[i].way == Way::Direct))
        && !clearly_better(c[pick].score, c[d].score)
    {
        pick = d;
    }
    if let Some(cur) = pool.iter().copied().find(|&i| c[i].current) {
        let to_direct = c[pick].way == Way::Direct && c[cur].way != Way::Direct && !clearly_better(c[cur].score, c[pick].score);
        if !clearly_better(c[pick].score, c[cur].score) && !to_direct {
            pick = cur;
        }
    }
    Some(pick)
}

fn score(s: &iroh::endpoint::PathStats) -> f64 {
    let sent = s.sent_packets.saturating_sub(s.sent_plpmtud_probes);
    let lost = s.lost_packets.saturating_sub(s.lost_plpmtud_probes);
    let loss = if sent >= LOSS_SAMPLE { lost as f64 / sent as f64 } else { 0.0 };
    s.rtt.as_secs_f64() * (1.0 + LOSS_WEIGHT * loss)
}

/// iroh asks this whenever a path opens or closes and when a connection
/// starts; see `choose`. iroh's own choice ranks only by round trip with
/// every relay alike, so a public relay could carry a session, and its
/// direct-first bias kept a direct path of 3 s over a relay of 200 ms.
#[derive(Debug)]
struct Prefer;

impl PathSelector for Prefer {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let current = ctx.current();
        let mut paths: Vec<PathSelectionData<'_>> = Vec::new();
        let mut cands: Vec<Candidate> = Vec::new();
        for psd in ctx.paths() {
            let Some(stats) = psd.stats() else { continue };
            let way = match psd.network_path().remote() {
                Addr::Relay(url, _) if public_index(&url).is_none() => Way::OwnRelay,
                Addr::Relay(..) => Way::PublicRelay,
                _ => Way::Direct,
            };
            let is_current = Some(psd.network_path()) == current;
            cands.push(Candidate { way, score: score(&stats), current: is_current });
            paths.push(psd);
        }
        let mut selection = PathSelection::none();
        if let Some(i) = choose(&cands)
            && !cands[i].current
        {
            selection.set(&paths[i]);
        }
        selection
    }
}

pub async fn endpoint(secret: SecretKey, relays: &Relays, ip: bool) -> Result<Endpoint> {
    let mut b = builder(secret, &[ALPN, crate::meet::ALPN, crate::term::ALPN]);
    // No relay of its own: an empty map, not RelayMode::Disabled, which
    // would also refuse the other side's relay named in a connection code.
    let map = relays.map().unwrap_or_else(iroh::RelayMap::empty);
    b = b.relay_mode(RelayMode::Custom(map));
    if !ip {
        b = b.clear_ip_transports();
    }
    b.bind().await.map_err(|e| anyhow!("bind: {e:#}"))
}

fn builder(secret: SecretKey, alpns: &[&[u8]]) -> iroh::endpoint::Builder {
    let b = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(alpns.iter().map(|a| a.to_vec()).collect())
        .transport_config(transport_config())
        .ca_tls_config(rc_relay::tls::ca())
        .path_selector(Arc::new(Prefer));
    match ipv6_detour() {
        Some(src) => {
            log!("IPv6 to the internet leaves from {src}, not a public address of this computer (a gateway in between, such as a proxy router): no IPv6 for direct connections");
            b.clear_ip_transports().bind_addr("0.0.0.0:0").expect("valid bind address")
        }
        None => b,
    }
}

/// The address IPv6 traffic to the internet would leave from, when that is
/// not a public one: the default IPv6 route goes through a gateway that is
/// not the one handing out the public prefix (a side router, often a
/// transparent proxy). Packets from outside still arrive at the public
/// address, but the answers take the detour and never come back as such.
/// A device dialing such an address sends its handshake on every path it
/// knows, this computer answers on the one that arrived first (that one),
/// and the handshake never completes, not even through a relay; the dialing
/// endpoint keeps the address and fails the same way until it restarts.
/// So such a computer takes no part in IPv6. Checked again when the
/// network changes (`node::v6_loop`): a laptop started at home behind such
/// a router has IPv6 of its own on mobile data.
pub fn ipv6_detour() -> Option<std::net::Ipv6Addr> {
    let s = std::net::UdpSocket::bind("[::]:0").ok()?;
    s.connect("[2001:4860:4860::8888]:53").ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V6(a) if !is_public_v6(a.ip()) => Some(*a.ip()),
        _ => None,
    }
}

fn is_public_v6(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xe000) == 0x2000
}

/// Whether the endpoint is connected to one of `relays` now.
pub fn connected_to(ep: &Endpoint, relays: &[RelayUrl]) -> bool {
    use iroh::Watcher;
    ep.home_relay_status().get().iter().any(|s| s.is_connected() && relays.contains(s.url()))
}

/// The relay this endpoint is reachable through now, if any.
pub fn home_relay(ep: &Endpoint) -> Option<RelayUrl> {
    ep.addr().relay_urls().next().cloned()
}

/// `%APPDATA%\remote-control\device.key` unless overridden.
pub fn default_key_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("remote-control").join("device.key")
}

/// Held by the one process whose endpoint is online with a device key. The
/// relay keeps one connection per device id: when a second endpoint with the
/// id connects, it tells the first "Another endpoint connected with the same
/// endpoint id" and sends it nothing more. Session windows use keys of their
/// own (see `ident`), so only a second lobby or host would clash. An
/// exclusively opened file next to the key; Windows lets go of it when the
/// process ends, however it ends.
pub struct IdLock(#[allow(dead_code)] std::fs::File);

impl IdLock {
    /// Waits up to `wait` for another process to let go.
    pub fn acquire(key: &Path, wait: Duration) -> Result<IdLock> {
        use std::os::windows::fs::OpenOptionsExt;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        let mut name = key.file_name().unwrap_or_default().to_os_string();
        name.push(".online");
        let path = key.with_file_name(name);
        let until = std::time::Instant::now() + wait;
        loop {
            match std::fs::OpenOptions::new().write(true).create(true).truncate(true).share_mode(0).open(&path) {
                Ok(f) => return Ok(IdLock(f)),
                Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                    if std::time::Instant::now() >= until {
                        return Err(anyhow!(
                            "another remote-control process is online with this device key ({})",
                            key.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(anyhow!("{}: {e}", path.display())),
            }
        }
    }
}

/// The device identity. Created on first use and kept, so the id stays the same.
pub fn load_key(p: &Path) -> Result<SecretKey> {
    if let Ok(b) = std::fs::read(p) {
        let arr: [u8; 32] = b.as_slice().try_into().context("key file must be 32 bytes")?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let k = SecretKey::generate();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(p, k.to_bytes()).with_context(|| format!("write {}", p.display()))?;
    Ok(k)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    None,
    Direct,
    /// Through a self-hosted relay.
    Relay,
    /// Through a public relay: fine for meeting, not for video.
    Public,
}

impl PathKind {
    pub fn code(self) -> u8 {
        match self {
            PathKind::None => 0,
            PathKind::Direct => 1,
            PathKind::Relay => 2,
            PathKind::Public => 3,
        }
    }

    pub fn from_code(c: u8) -> PathKind {
        match c {
            1 => PathKind::Direct,
            2 => PathKind::Relay,
            3 => PathKind::Public,
            _ => PathKind::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PathKind::None => "无通路",
            PathKind::Direct => "直连",
            PathKind::Relay => "中转",
            PathKind::Public => "公共节点",
        }
    }
}

/// Kind, address and round trip in ms of the path used for sending.
pub fn selected_path(conn: &Connection) -> (PathKind, String, f64) {
    for p in conn.paths().iter() {
        if p.is_selected() {
            let rtt = p.rtt().as_secs_f64() * 1000.0;
            return match p.remote_addr() {
                TransportAddr::Relay(u) if public_index(u).is_some() => (PathKind::Public, u.to_string(), rtt),
                TransportAddr::Relay(u) => (PathKind::Relay, u.to_string(), rtt),
                TransportAddr::Ip(sa) => (PathKind::Direct, describe_ip(sa), rtt),
                other => (PathKind::Direct, format!("{other:?}"), rtt),
            };
        }
    }
    (PathKind::None, String::new(), 0.0)
}

/// Whether the connection has a direct path open, selected or not.
pub fn has_direct_path(conn: &Connection) -> bool {
    conn.paths().iter().any(|p| matches!(p.remote_addr(), TransportAddr::Ip(_)))
}

/// The direct address the peer is talking from, if it talks directly.
pub fn direct_addr(conn: &Connection) -> Option<SocketAddr> {
    conn.paths().iter().find_map(|p| match p.remote_addr() {
        TransportAddr::Ip(sa) if p.is_selected() => Some(*sa),
        _ => None,
    })
}

/// Congestion state of the selected path: (window bytes, bandwidth estimate
/// bits/s, packets lost so far, congestion events so far).
pub fn path_congestion(conn: &Connection) -> Option<(u64, Option<u64>, u64, u64)> {
    let paths = conn.paths();
    let p = paths.iter().find(|p| p.is_selected())?;
    let s = p.stats();
    Some((s.cwnd, s.bandwidth_estimate, s.lost_packets, s.congestion_events))
}

pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local() || v6.is_unique_local(),
    }
}

fn describe_ip(sa: &SocketAddr) -> String {
    format!("{}{sa}", if is_lan(sa.ip()) { "局域网 " } else { "" })
}

pub fn dial_addr(id: EndpointId, ips: &[SocketAddr], relays: &[RelayUrl]) -> EndpointAddr {
    rc_relay::tls::learn_all(relays);
    let mut t: Vec<TransportAddr> = ips.iter().copied().map(TransportAddr::Ip).collect();
    t.extend(relays.iter().cloned().map(TransportAddr::Relay));
    EndpointAddr::from_parts(id, t)
}

#[cfg(test)]
mod tests {
    use super::{Candidate, Way, choose};

    fn c(way: Way, ms: f64, current: bool) -> Candidate {
        Candidate { way, score: ms / 1000.0, current }
    }

    #[test]
    fn the_better_path_wins_whatever_its_kind() {
        let slow_direct = [c(Way::OwnRelay, 200.0, true), c(Way::Direct, 2980.0, false)];
        assert_eq!(choose(&slow_direct), Some(0), "a 3 s direct path does not beat a 200 ms relay");
        let fast_direct = [c(Way::OwnRelay, 200.0, true), c(Way::Direct, 60.0, false)];
        assert_eq!(choose(&fast_direct), Some(1));
        let from_slow_direct = [c(Way::Direct, 2980.0, true), c(Way::OwnRelay, 200.0, false)];
        assert_eq!(choose(&from_slow_direct), Some(1), "leaves a direct path that turned slow");
    }

    #[test]
    fn about_as_good_goes_direct_and_otherwise_stays() {
        let close = [c(Way::OwnRelay, 200.0, true), c(Way::Direct, 210.0, false)];
        assert_eq!(choose(&close), Some(1), "about as good: direct, which costs the relay nothing");
        let stay = [c(Way::Direct, 200.0, true), c(Way::OwnRelay, 190.0, false)];
        assert_eq!(choose(&stay), Some(0), "10 ms less is not clearly better");
        let two_direct = [c(Way::Direct, 100.0, true), c(Way::Direct, 96.0, false)];
        assert_eq!(choose(&two_direct), Some(0), "no flapping between direct paths");
        let first = [c(Way::OwnRelay, 200.0, false), c(Way::Direct, 205.0, false)];
        assert_eq!(choose(&first), Some(1));
    }

    #[test]
    fn a_public_relay_only_while_nothing_else_is_open() {
        let only = [c(Way::PublicRelay, 300.0, true)];
        assert_eq!(choose(&only), Some(0));
        let slower_own = [c(Way::PublicRelay, 150.0, true), c(Way::OwnRelay, 400.0, false)];
        assert_eq!(choose(&slower_own), Some(1));
        assert_eq!(choose(&[]), None);
    }
}
