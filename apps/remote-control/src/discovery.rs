//! LAN discovery. A computer answers queries on UDP port `PORT` with its
//! device id, name and iroh port; nothing secret. The lobby broadcasts a
//! query on every IPv4 network every few seconds and collects the answers.
//! The query comes from the asker (not the host announcing itself) because
//! Windows Firewall lets unicast answers to one's own broadcast back in, so
//! only the answering side needs an inbound rule, which it needs for direct
//! connections anyway. A query carries the asker's device id, so a computer
//! whose window is closed (service mode) can answer only devices it lists.

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use iroh::{Endpoint, EndpointId, PublicKey};
use tokio::net::UdpSocket;

use crate::log;

pub const PORT: u16 = 47821;
const MAGIC: &[u8; 4] = b"RCD1";
const QUERY: u8 = 1;
const ANSWER: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub id: EndpointId,
    pub name: String,
    /// The answer's source address with the answering iroh port.
    pub addr: SocketAddr,
}

fn query_packet(asker: &EndpointId) -> Vec<u8> {
    let mut v = MAGIC.to_vec();
    v.push(QUERY);
    v.extend_from_slice(asker.as_bytes());
    v
}

fn answer_packet(id: &EndpointId, port: u16, name: &str) -> Vec<u8> {
    let mut v = MAGIC.to_vec();
    v.push(ANSWER);
    v.extend_from_slice(id.as_bytes());
    v.extend_from_slice(&port.to_be_bytes());
    let mut n = name.len().min(64);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    v.push(n as u8);
    v.extend_from_slice(&name.as_bytes()[..n]);
    v
}

fn parse_answer(b: &[u8], from: SocketAddr) -> Option<Found> {
    let rest = b.strip_prefix(MAGIC)?.strip_prefix(&[ANSWER])?;
    let id: [u8; 32] = rest.get(..32)?.try_into().ok()?;
    let id = PublicKey::from_bytes(&id).ok()?;
    let port = u16::from_be_bytes(rest.get(32..34)?.try_into().ok()?);
    let n = *rest.get(34)? as usize;
    let name = String::from_utf8_lossy(rest.get(35..35 + n)?).into_owned();
    Some(Found { id, name, addr: SocketAddr::new(from.ip(), port) })
}

/// A query, and who asked (None from versions that did not say).
fn parse_query(b: &[u8]) -> Option<Option<EndpointId>> {
    let rest = b.strip_prefix(MAGIC)?.strip_prefix(&[QUERY])?;
    let asker = rest.get(..32).and_then(|k| PublicKey::from_bytes(k.try_into().ok()?).ok());
    Some(asker)
}

/// The IPv4 port iroh listens on.
fn iroh_port(ep: &Endpoint) -> u16 {
    ep.bound_sockets().iter().find(|a| a.is_ipv4()).map_or(0, |a| a.port())
}

/// Decides per asker whether to answer, and with which name.
pub type Answerer = Arc<dyn Fn(Option<EndpointId>) -> Option<String> + Send + Sync>;

/// Answers queries until the endpoint closes. Logs and gives up if the port
/// is taken (another remote-control on this computer answers already).
pub fn respond(ep: Endpoint, answer: Answerer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tries = 0;
        let sock = loop {
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT)).await {
                Ok(s) => break s,
                Err(_) if tries < 10 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(e) => return log!("LAN discovery off: UDP port {PORT}: {e}"),
            }
        };
        log!("answering LAN discovery on UDP port {PORT}");
        let (id, port) = (ep.id(), iroh_port(&ep));
        let mut buf = [0u8; 512];
        loop {
            tokio::select! {
                r = sock.recv_from(&mut buf) => match r {
                    Ok((n, from)) => {
                        if let Some(asker) = parse_query(&buf[..n])
                            && asker != Some(id)
                            && let Some(name) = answer(asker)
                        {
                            let _ = sock.send_to(&answer_packet(&id, port, &name), from).await;
                        }
                    }
                    // Windows reports an earlier send's ICMP "port unreachable" here.
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
                _ = ep.closed() => return,
            }
        }
    })
}

/// This computer's IPv4 networks: (own address, broadcast address).
fn networks() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut v = Vec::new();
    for i in netdev::get_interfaces() {
        if !i.is_up() || i.is_loopback() {
            continue;
        }
        for n in &i.ipv4 {
            if n.prefix_len() < 32 && !n.addr().is_link_local() {
                v.push((n.addr(), n.broadcast()));
            }
        }
    }
    v
}

/// Broadcasts one query on every network and collects answers for `wait`.
/// Answers from this computer are left out.
pub async fn scan(wait: Duration, me: &EndpointId) -> Result<Vec<Found>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sock.set_broadcast(true)?;
    let q = query_packet(me);
    let nets = networks();
    let mut targets: Vec<Ipv4Addr> = nets.iter().map(|n| n.1).collect();
    targets.push(Ipv4Addr::BROADCAST);
    for t in targets {
        let _ = sock.send_to(&q, (t, PORT)).await;
    }
    let mine: Vec<IpAddr> = nets.iter().map(|n| IpAddr::V4(n.0)).collect();
    let mut found = BTreeMap::new();
    let end = Instant::now() + wait;
    let mut buf = [0u8; 512];
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Some(f) = parse_answer(&buf[..n], from)
                    && !mine.contains(&from.ip())
                    && f.id != *me
                {
                    found.entry(f.id).or_insert(f);
                }
            }
            Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(_) => break,
        }
    }
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip() {
        let id = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let from: SocketAddr = "192.168.0.5:47821".parse().unwrap();
        let p = answer_packet(&id, 50123, "笔记本");
        let f = parse_answer(&p, from).unwrap();
        assert_eq!(f, Found { id, name: "笔记本".into(), addr: "192.168.0.5:50123".parse().unwrap() });
        assert!(parse_answer(&p[..20], from).is_none());
        let q = query_packet(&id);
        assert!(parse_answer(&q, from).is_none());
        assert_eq!(parse_query(&q), Some(Some(id)));
        // A query from phase 1, without the asker.
        assert_eq!(parse_query(&q[..5]), Some(None));
        assert_eq!(parse_query(&p), None);
    }
}
