//! LAN UDP discovery packets (`RCD1`, port 47821).

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::Result;
use iroh_base::PublicKey;
use tokio::net::UdpSocket;

pub const PORT: u16 = 47821;
const MAGIC: &[u8; 4] = b"RCD1";
const QUERY: u8 = 1;
const ANSWER: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub id: PublicKey,
    pub name: String,
    pub addr: SocketAddr,
}

pub fn query_packet(asker: &PublicKey) -> Vec<u8> {
    let mut v = MAGIC.to_vec();
    v.push(QUERY);
    v.extend_from_slice(asker.as_bytes());
    v
}

pub fn parse_answer(b: &[u8], from: SocketAddr) -> Option<Found> {
    let rest = b.strip_prefix(MAGIC)?.strip_prefix(&[ANSWER])?;
    let id: [u8; 32] = rest.get(..32)?.try_into().ok()?;
    let id = PublicKey::from_bytes(&id).ok()?;
    let port = u16::from_be_bytes(rest.get(32..34)?.try_into().ok()?);
    let n = *rest.get(34)? as usize;
    let name = String::from_utf8_lossy(rest.get(35..35 + n)?).into_owned();
    Some(Found {
        id,
        name,
        addr: SocketAddr::new(from.ip(), port),
    })
}

/// Broadcast a LAN discovery query to `targets` (plus `255.255.255.255`) and collect replies for `wait`.
pub async fn scan_targets(
    wait: Duration,
    me: &PublicKey,
    extra_broadcasts: &[Ipv4Addr],
) -> Result<Vec<Found>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sock.set_broadcast(true)?;
    let q = query_packet(me);

    let mut targets = Vec::with_capacity(extra_broadcasts.len() + 1);
    for &b in extra_broadcasts {
        if !targets.contains(&b) {
            targets.push(b);
        }
    }
    if !targets.contains(&Ipv4Addr::BROADCAST) {
        targets.push(Ipv4Addr::BROADCAST);
    }

    for t in targets {
        let _ = sock.send_to(&q, (t, PORT)).await;
    }

    let mut found = BTreeMap::new();
    let end = Instant::now() + wait;
    let mut buf = [0u8; 512];
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Some(f) = parse_answer(&buf[..n], from)
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
