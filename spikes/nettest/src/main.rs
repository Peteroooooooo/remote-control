use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey, TransportAddr,
    endpoint::{Connection, QuicTransportConfig, presets},
};
use rc_stream::{
    fec::Assembler,
    wire::{Ack, T_SHARD},
};

mod cc;
mod dial;

pub const ALPN: &[u8] = b"rc/nettest/1";
pub const DEFAULT_RELAY_URL: &str = "https://relay.example.com:7843";
pub const INPUT_MSG: usize = 16;
/// How often the receiver sends a loss report and the sender updates its bitrate.
pub const REPORT_EVERY: Duration = Duration::from_millis(100);
/// How long the receiver keeps a frame open for late shards before reporting it.
const FRAME_LINGER: Duration = Duration::from_millis(150);

#[derive(Parser)]
#[command(about = "iroh network test for the remote-control project (phase 0)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wait for connections, rebuild simulated video frames and acknowledge them.
    Listen {
        #[arg(long, default_value = DEFAULT_RELAY_URL)]
        relay: String,
        /// File holding this listener's secret key, so its id stays the same between runs.
        #[arg(long, default_value = "nettest.key")]
        key: PathBuf,
    },
    /// Connect to a listener and send simulated video plus input traffic.
    Dial(dial::DialArgs),
}

/// QUIC congestion controller. Datagrams count against its window too.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Cc {
    /// Loss-based; the QUIC default.
    Cubic,
    /// Model-based; tolerates random loss better.
    Bbr,
    Newreno,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    fine_timers();
    match Cli::parse().cmd {
        Cmd::Listen { relay, key } => listen(relay.parse()?, &key).await,
        Cmd::Dial(args) => dial::run(args).await,
    }
}

/// Windows timers tick every 15.6 ms by default, too coarse for pacing.
#[cfg(windows)]
fn fine_timers() {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    unsafe {
        timeBeginPeriod(1);
    }
}

#[cfg(not(windows))]
fn fine_timers() {}

pub fn transport_config(
    cc: Cc,
    send_buffer: Option<usize>,
    window_gain: f64,
    stats: Arc<cc::CcStats>,
) -> QuicTransportConfig {
    use noq_proto::congestion::{Bbr3Config, ControllerFactory, CubicConfig, NewRenoConfig};
    let inner: Arc<dyn ControllerFactory + Send + Sync> = match cc {
        Cc::Cubic => Arc::new(CubicConfig::default()),
        Cc::Bbr => Arc::new(Bbr3Config::default()),
        Cc::Newreno => Arc::new(NewRenoConfig::default()),
    };
    let mut b = QuicTransportConfig::builder()
        .congestion_controller_factory(Arc::new(cc::Factory { inner, window_gain, stats }));
    if let Some(n) = send_buffer {
        b = b.datagram_send_buffer_size(n);
    }
    b.build()
}

pub async fn endpoint(
    secret: SecretKey,
    relay: Option<RelayUrl>,
    ip: bool,
    transport: QuicTransportConfig,
) -> Result<Endpoint> {
    let mut b = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport);
    b = match relay {
        Some(u) => b.relay_mode(RelayMode::custom([u])),
        None => b.relay_mode(RelayMode::Disabled),
    };
    if !ip {
        b = b.clear_ip_transports();
    }
    b.bind().await.map_err(|e| anyhow!("bind: {e:#}"))
}

fn load_key(p: &Path) -> Result<SecretKey> {
    if let Ok(b) = std::fs::read(p) {
        let arr: [u8; 32] = b.as_slice().try_into().context("key file must be 32 bytes")?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let k = SecretKey::generate();
    std::fs::write(p, k.to_bytes()).context("write key file")?;
    Ok(k)
}

fn classify(a: &TransportAddr) -> (&'static str, String) {
    match a {
        TransportAddr::Relay(u) => ("relay", u.to_string()),
        TransportAddr::Ip(sa) => {
            let kind = match sa.ip() {
                IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => "lan4",
                IpAddr::V4(_) => "direct4",
                IpAddr::V6(v6) if v6.is_unicast_link_local() || v6.is_unique_local() => "lan6",
                IpAddr::V6(_) => "direct6",
            };
            (kind, sa.to_string())
        }
        other => ("other", format!("{other:?}")),
    }
}

/// (kind, address, rtt in ms) of the path currently used for sending.
pub fn selected_path(conn: &Connection) -> (&'static str, String, f64) {
    let paths = conn.paths();
    for p in paths.iter() {
        if p.is_selected() {
            let (k, a) = classify(p.remote_addr());
            return (k, a, p.rtt().as_secs_f64() * 1000.0);
        }
    }
    ("none", String::new(), 0.0)
}

pub fn make_dial_addr(id: EndpointId, ips: &[SocketAddr], relay: Option<&RelayUrl>) -> EndpointAddr {
    let mut t: Vec<TransportAddr> = ips.iter().copied().map(TransportAddr::Ip).collect();
    if let Some(u) = relay {
        t.push(TransportAddr::Relay(u.clone()));
    }
    EndpointAddr::from_parts(id, t)
}

async fn listen(relay: RelayUrl, key: &Path) -> Result<()> {
    let ep = endpoint(load_key(key)?, Some(relay), true, transport_config(Cc::Bbr, None, 1.0, Default::default())).await?;
    if tokio::time::timeout(Duration::from_secs(10), ep.online()).await.is_err() {
        eprintln!("warning: relay not reachable after 10 s; only direct addresses will work");
    }
    let a = ep.addr();
    println!("id:    {}", ep.id());
    for ip in a.ip_addrs() {
        println!("addr:  {ip}");
    }
    for r in a.relay_urls() {
        println!("relay: {r}");
    }
    println!("\ndial with:\n  nettest dial {}\n", ep.id());
    while let Some(incoming) = ep.accept().await {
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    let peer = conn.remote_id().fmt_short().to_string();
                    println!("[{peer}] connected");
                    if let Err(e) = serve(conn, &peer).await {
                        println!("[{peer}] ended: {e:#}");
                    }
                }
                Err(e) => println!("incoming failed: {e:#}"),
            }
        });
    }
    Ok(())
}

async fn serve(conn: Connection, peer: &str) -> Result<()> {
    let watch = conn.clone();
    let tag = peer.to_string();
    tokio::spawn(async move {
        let mut last = String::new();
        while watch.close_reason().is_none() {
            let (k, a, rtt) = selected_path(&watch);
            let cur = format!("{k} {a}");
            if cur != last {
                println!("[{tag}] path -> {cur} (rtt {rtt:.1} ms)");
                last = cur;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let echo = conn.clone();
    tokio::spawn(async move {
        let Ok((mut send, mut recv)) = echo.accept_bi().await else { return };
        let mut buf = [0u8; INPUT_MSG];
        while recv.read_exact(&mut buf).await.is_ok() {
            if send.write_all(&buf).await.is_err() {
                break;
            }
        }
    });
    // Frames are acked the moment they can be decoded; nothing waits for ordering.
    let mut asm = Assembler::new(FRAME_LINGER);
    let mut tick = tokio::time::interval(REPORT_EVERY);
    loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let d = d?;
                if d.first() != Some(&T_SHARD) {
                    continue;
                }
                if let Some(c) = asm.push(&d, Instant::now()) {
                    let ack = Ack { frame: c.frame, recovered: c.recovered, resent: c.resent, prev_mask: 0 };
                    let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(&ack.encode()));
                }
            }
            _ = tick.tick() => {
                asm.expire(Instant::now());
                let r = asm.take_report();
                if r.shards_expected > 0 {
                    let _ = conn.send_datagram(r.encode().into());
                }
            }
        }
    }
}

pub type Shared<T> = Arc<Mutex<T>>;
pub fn elapsed_ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}
