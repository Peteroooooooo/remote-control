//! Checks a deployed rc-relay from outside, with endpoints that can only
//! reach each other through it.
//!
//! ```text
//! cargo run --example relay_check -- keygen owner.key     # prints the id to add as an owner
//! cargo run --example relay_check -- run https://relay.example.com:7843 owner.key [MB]
//! ```
//!
//! `run` expects the key in `owner.key` to own the relay (use a throwaway
//! owner: the check replaces that owner's guest list, and empties it at the
//! end). It checks: a stranger does not reach the owner; once listed, a
//! guest reaches the owner and back, and so does a session key with the
//! guest's device proof; a guest does not reach a stranger or another
//! guest; a guest taken off the list stops at once; and the rate each way
//! between owner and guest.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{
    Endpoint, EndpointAddr, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr, endpoint::presets,
};
use rc_ident::proof::{DeviceProof, unix_now};

const ALPN: &[u8] = b"rc-relay/check";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["keygen", path] => {
            let k = SecretKey::generate();
            std::fs::write(path, k.to_bytes()).with_context(|| format!("write {path}"))?;
            println!("{}", k.public());
            Ok(())
        }
        ["run", url, key, ..] => {
            let mb: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(16);
            let url: RelayUrl = url.parse().map_err(|e| anyhow!("{url}: {e}"))?;
            let b = std::fs::read(key).with_context(|| format!("read {key}"))?;
            let owner = SecretKey::from_bytes(&b.as_slice().try_into().map_err(|_| anyhow!("{key}: not 32 bytes"))?);
            run(url, owner, mb * 1_000_000).await
        }
        _ => bail!("usage: relay_check keygen <file> | run <relay url> <owner key file> [MB]"),
    }
}

async fn run(url: RelayUrl, owner: SecretKey, bytes: usize) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    rc_relay::tls::learn(&url);
    let ca = rc_relay::tls::ca();
    let (guest, guest2) = (SecretKey::generate(), SecretKey::generate());
    rc_relay::client::set_guests(std::slice::from_ref(&url), &owner, &ca, &[guest.public().into(), guest2.public().into()]).await?;
    let o = endpoint(&url, owner.clone(), None).await?;
    let g = endpoint(&url, guest.clone(), None).await?;
    let g2 = endpoint(&url, guest2, None).await?;
    let x = endpoint(&url, SecretKey::generate(), None).await?;
    let e = SecretKey::generate();
    let proof = DeviceProof::issue(&guest, e.public(), unix_now() + 3600).encode();
    let gp = endpoint(&url, e, Some(proof)).await?;

    let mut ok = true;
    let mut check = async |what: &str, from: &Endpoint, to: &Endpoint, want: bool| {
        let got = reaches(&url, from, to).await;
        ok &= got == want;
        println!(
            "{what}: {} ({})",
            if got { "connected" } else { "no connection" },
            if got == want { "as expected" } else { "WRONG" }
        );
    };
    check("stranger → owner", &x, &o, false).await;
    check("owner → stranger", &o, &x, false).await;
    check("guest → owner", &g, &o, true).await;
    check("owner → guest", &o, &g, true).await;
    check("guest's session key → owner", &gp, &o, true).await;
    check("guest → stranger", &g, &x, false).await;
    check("guest → other guest", &g, &g2, false).await;
    let up = send_rate(&url, &g, &o, bytes).await?;
    let down = send_rate(&url, &o, &g, bytes).await?;
    println!("guest → owner {:.1} Mbit/s; owner → guest {:.1} Mbit/s ({} MB each)", up * 8.0, down * 8.0, bytes / 1_000_000);
    rc_relay::client::set_guests(std::slice::from_ref(&url), &owner, &ca, &[]).await?;
    check("guest after it was taken off the list → owner", &g, &o, false).await;
    for ep in [o, g, g2, x, gp] {
        ep.close().await;
    }
    if !ok {
        bail!("the relay does not follow its rules");
    }
    Ok(())
}

async fn endpoint(url: &RelayUrl, key: SecretKey, token: Option<String>) -> Result<Endpoint> {
    let mut rc = RelayConfig::new(url.clone(), None);
    if let Some(t) = token {
        rc = rc.with_auth_token(t);
    }
    let ep = Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom(RelayMap::from_iter([rc])))
        .ca_tls_config(rc_relay::tls::ca())
        .clear_ip_transports()
        .bind()
        .await?;
    tokio::time::timeout(Duration::from_secs(15), ep.online()).await.context("could not reach the relay")?;
    tokio::spawn(serve(ep.clone()));
    Ok(ep)
}

fn addr(url: &RelayUrl, ep: &Endpoint) -> EndpointAddr {
    EndpointAddr::from_parts(ep.id(), [TransportAddr::Relay(url.clone())])
}

async fn reaches(url: &RelayUrl, from: &Endpoint, to: &Endpoint) -> bool {
    let attempt = async {
        let conn = from.connect(addr(url, to), ALPN).await.ok()?;
        let (mut s, mut r) = conn.open_bi().await.ok()?;
        s.write_all(b"ping").await.ok()?;
        s.finish().ok()?;
        let back = r.read_to_end(16).await.ok()?;
        conn.close(0u32.into(), b"");
        (back == b"ping").then_some(())
    };
    tokio::time::timeout(Duration::from_secs(10), attempt).await.ok().flatten().is_some()
}

/// MB/s for `bytes` on one stream, counted until the other end has them all.
async fn send_rate(url: &RelayUrl, from: &Endpoint, to: &Endpoint, bytes: usize) -> Result<f64> {
    let conn = from.connect(addr(url, to), ALPN).await?;
    let (mut s, mut r) = conn.open_bi().await?;
    let t = Instant::now();
    s.write_all(&vec![7u8; bytes]).await?;
    s.finish()?;
    let back = r.read_to_end(64).await?;
    let secs = t.elapsed().as_secs_f64();
    if back != bytes.to_string().into_bytes() {
        bail!("the other end got a different amount");
    }
    conn.close(0u32.into(), b"");
    Ok(bytes as f64 / 1e6 / secs)
}

/// Answers "ping" with "ping" and anything else with its length.
async fn serve(ep: Endpoint) {
    while let Some(inc) = ep.accept().await {
        tokio::spawn(async move {
            let Ok(conn) = inc.await else { return };
            while let Ok((mut s, mut r)) = conn.accept_bi().await {
                let Ok(data) = r.read_to_end(256 << 20).await else { return };
                let answer = if data == b"ping" { data } else { data.len().to_string().into_bytes() };
                let _ = s.write_all(&answer).await;
                let _ = s.finish();
                let _ = s.stopped().await;
            }
        });
    }
}
