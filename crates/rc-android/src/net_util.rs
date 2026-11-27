//! iroh endpoint configuration and path selection matching the Windows host.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey,
    TransportAddr,
    endpoint::{
        Connection, IdleTimeout, QuicTransportConfig, presets,
        transports::{Addr, PathSelection, PathSelectionContext, PathSelectionData, PathSelector},
    },
};
use rc_proto::proto::{ALPN, MEET_ALPN};

pub const PUBLIC_RELAYS: [&str; 4] = [
    "https://use1-1.relay.n0.iroh.link./",
    "https://usw1-1.relay.n0.iroh.link./",
    "https://euc1-1.relay.n0.iroh.link./",
    "https://aps1-1.relay.n0.iroh.link./",
];

pub const SEND_BUFFER: usize = 4 * 1024 * 1024;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(1);

pub fn public_relays() -> Vec<RelayUrl> {
    PUBLIC_RELAYS
        .iter()
        .map(|u| u.parse().expect("valid public relay URL"))
        .collect()
}

pub fn public_index(url: &RelayUrl) -> Option<u8> {
    let host = url.host_str()?.trim_end_matches('.');
    PUBLIC_RELAYS
        .iter()
        .position(|p| p.contains(&format!("//{host}.")))
        .map(|i| i as u8)
}

pub fn public_relay(n: u8) -> Option<RelayUrl> {
    PUBLIC_RELAYS
        .get(n as usize)
        .map(|u| u.parse().expect("valid public relay URL"))
}

pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .datagram_send_buffer_size(SEND_BUFFER)
        .max_idle_timeout(Some(
            IdleTimeout::try_from(IDLE_TIMEOUT).expect("idle timeout fits"),
        ))
        .keep_alive_interval(KEEP_ALIVE)
        .build()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    None,
    Direct,
    Relay,
    Public,
}

impl PathKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PathKind::None => "none",
            PathKind::Direct => "direct",
            PathKind::Relay => "relay",
            PathKind::Public => "public",
        }
    }
}

const LOSS_WEIGHT: f64 = 10.0;
const LOSS_SAMPLE: u64 = 50;
const SWITCH_SHARE: f64 = 0.10;
const SWITCH_MIN: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    Direct,
    OwnRelay,
    PublicRelay,
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    way: Way,
    score: f64,
    current: bool,
}

fn clearly_better(a: f64, b: f64) -> bool {
    a + (b * SWITCH_SHARE).max(SWITCH_MIN.as_secs_f64()) < b
}

fn choose(c: &[Candidate]) -> Option<usize> {
    let pool: Vec<usize> = match (0..c.len())
        .filter(|&i| c[i].way != Way::PublicRelay)
        .collect::<Vec<_>>()
    {
        v if v.is_empty() => (0..c.len()).collect(),
        v => v,
    };
    let lowest = |ix: &mut dyn Iterator<Item = usize>| {
        ix.min_by(|&a, &b| c[a].score.total_cmp(&c[b].score))
    };
    let mut pick = lowest(&mut pool.iter().copied())?;
    if c[pick].way != Way::Direct
        && let Some(d) = lowest(&mut pool.iter().copied().filter(|&i| c[i].way == Way::Direct))
        && !clearly_better(c[pick].score, c[d].score)
    {
        pick = d;
    }
    if let Some(cur) = pool.iter().copied().find(|&i| c[i].current) {
        let to_direct = c[pick].way == Way::Direct
            && c[cur].way != Way::Direct
            && !clearly_better(c[cur].score, c[pick].score);
        if !clearly_better(c[pick].score, c[cur].score) && !to_direct {
            pick = cur;
        }
    }
    Some(pick)
}

fn score(s: &iroh::endpoint::PathStats) -> f64 {
    let sent = s.sent_packets.saturating_sub(s.sent_plpmtud_probes);
    let lost = s.lost_packets.saturating_sub(s.lost_plpmtud_probes);
    let loss = if sent >= LOSS_SAMPLE {
        lost as f64 / sent as f64
    } else {
        0.0
    };
    s.rtt.as_secs_f64() * (1.0 + LOSS_WEIGHT * loss)
}

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
            cands.push(Candidate {
                way,
                score: score(&stats),
                current: is_current,
            });
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

pub fn has_self_hosted_relay(own: &[RelayUrl], extra: &[RelayUrl]) -> bool {
    !own.is_empty() || extra.iter().any(|u| public_index(u).is_none())
}

pub fn self_hosted_relays(own: &[RelayUrl], extra: &[RelayUrl]) -> Vec<RelayUrl> {
    let mut out = Vec::new();
    for u in own.iter().chain(extra.iter()) {
        if public_index(u).is_none() && !out.contains(u) {
            out.push(u.clone());
        }
    }
    out
}

pub fn build_relay_map(own: &[RelayUrl], extra: &[RelayUrl]) -> RelayMap {
    rc_relay::tls::learn_all(own.iter().chain(extra));
    let mut configs: Vec<RelayConfig> = Vec::new();
    if !own.is_empty() {
        configs.extend(own.iter().cloned().map(RelayConfig::from));
    } else {
        configs.extend(public_relays().into_iter().map(RelayConfig::from));
    }
    for u in extra {
        if public_index(u).is_none() && !own.contains(u) {
            configs.push(RelayConfig::from(u.clone()));
        }
    }
    if configs.is_empty() {
        RelayMap::empty()
    } else {
        RelayMap::from_iter(configs)
    }
}

pub fn build_session_relay_map(
    own: &[RelayUrl],
    extra: &[RelayUrl],
    relay_only: bool,
    token: &str,
) -> RelayMap {
    rc_relay::tls::learn_all(own.iter().chain(extra));
    let cfg = |u: &RelayUrl| RelayConfig::from(u.clone()).with_auth_token(token.to_string());
    let mut configs: Vec<RelayConfig> = Vec::new();
    if relay_only {
        for u in self_hosted_relays(own, extra) {
            configs.push(cfg(&u));
        }
    }
    if configs.is_empty() {
        if !own.is_empty() {
            configs.extend(own.iter().map(cfg));
        } else {
            configs.extend(public_relays().into_iter().map(RelayConfig::from));
        }
        for u in extra {
            if public_index(u).is_none() && !own.contains(u) {
                configs.push(cfg(u));
            }
        }
    }
    if configs.is_empty() {
        RelayMap::empty()
    } else {
        RelayMap::from_iter(configs)
    }
}

pub async fn create_endpoint(
    secret: SecretKey,
    own_relays: &[RelayUrl],
    extra_relays: &[RelayUrl],
) -> Result<Endpoint> {
    let map = build_relay_map(own_relays, extra_relays);
    Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec(), MEET_ALPN.to_vec()])
        .transport_config(transport_config())
        .ca_tls_config(rc_relay::tls::ca())
        .path_selector(Arc::new(Prefer))
        .relay_mode(RelayMode::Custom(map))
        .bind()
        .await
        .map_err(|e| anyhow!("endpoint bind: {e:#}"))
}

pub async fn create_session_endpoint(
    secret: SecretKey,
    own_relays: &[RelayUrl],
    extra_relays: &[RelayUrl],
    relay_only: bool,
    token: &str,
) -> Result<Endpoint> {
    let map = build_session_relay_map(own_relays, extra_relays, relay_only, token);
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport_config())
        .ca_tls_config(rc_relay::tls::ca())
        .path_selector(Arc::new(Prefer))
        .relay_mode(RelayMode::Custom(map));
    if relay_only {
        builder = builder.clear_ip_transports();
    }
    builder
        .bind()
        .await
        .map_err(|e| anyhow!("session endpoint bind: {e:#}"))
}

pub fn endpoint_addr(
    id: EndpointId,
    addrs: &[SocketAddr],
    relays: &[RelayUrl],
) -> EndpointAddr {
    let mut ea = EndpointAddr::new(id);
    for &a in addrs {
        ea = ea.with_ip_addr(a);
    }
    for r in relays {
        ea = ea.with_relay_url(r.clone());
    }
    ea
}

pub fn selected_path(conn: &Connection) -> (PathKind, f64) {
    for p in conn.paths().iter() {
        if p.is_selected() {
            let rtt = p.rtt().as_secs_f64() * 1000.0;
            let kind = match p.remote_addr() {
                TransportAddr::Relay(u) if public_index(u).is_some() => PathKind::Public,
                TransportAddr::Relay(_) => PathKind::Relay,
                TransportAddr::Ip(_) => PathKind::Direct,
                _ => PathKind::Direct,
            };
            return (kind, rtt);
        }
    }
    (PathKind::None, 0.0)
}

pub fn direct_addr(conn: &Connection) -> Option<SocketAddr> {
    for p in conn.paths().iter() {
        if let TransportAddr::Ip(sa) = p.remote_addr() {
            return Some(*sa);
        }
    }
    None
}

pub fn url_list(urls: &[RelayUrl]) -> String {
    urls.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn parse_relay_urls(s: &str) -> Vec<RelayUrl> {
    s.split([',', ' '])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .filter_map(|x| x.parse::<RelayUrl>().ok())
        .collect()
}
