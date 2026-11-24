use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use clap::Args;
use iroh::{EndpointId, RelayUrl, SecretKey, endpoint::Connection};
use rc_stream::{
    budget::{Budget, Frame},
    fec::encode_frame,
    pacer,
    rate::{RateConfig, RateController},
    wire::{Ack, Report, T_ACK, T_REPORT},
};

use crate::{Cc, DEFAULT_RELAY_URL, INPUT_MSG, REPORT_EVERY, Shared, elapsed_ms};

/// A frame not acked within this time counts as failed.
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);
/// Gap between paced batches.
const PACE_GAP: Duration = Duration::from_millis(1);
/// Parity shards added even to one-datagram frames when FEC is on.
const MIN_PARITY: u16 = 2;

#[derive(Args)]
pub struct DialArgs {
    /// Listener id printed by `nettest listen`.
    id: EndpointId,
    /// Direct address of the listener (repeatable). Works without any server.
    #[arg(long)]
    addr: Vec<SocketAddr>,
    #[arg(long, default_value = DEFAULT_RELAY_URL)]
    relay: String,
    /// Force all traffic through the relay.
    #[arg(long)]
    relay_only: bool,
    /// Do not use any relay (only --addr).
    #[arg(long, conflicts_with = "relay_only")]
    no_relay: bool,
    /// Highest (and starting) video bitrate.
    #[arg(long, default_value_t = 20.0)]
    mbps: f64,
    /// Lowest bitrate the controller may pick.
    #[arg(long, default_value_t = 2.0)]
    min_mbps: f64,
    /// Always send at --mbps instead of following delay and loss.
    #[arg(long)]
    fixed_rate: bool,
    /// Parity shards as a percentage of data shards, or "auto" to follow measured loss.
    #[arg(long, default_value = "auto")]
    fec: String,
    /// Spread each frame's datagrams over this fraction of the frame interval (0 = one burst).
    #[arg(long, default_value_t = 0.25)]
    pace: f64,
    #[arg(long, value_enum, default_value = "bbr")]
    cc: Cc,
    /// Allow this many times the congestion controller's window in flight.
    #[arg(long, default_value_t = 1.0)]
    window_gain: f64,
    /// Queued data allowed right after a frame is handed over, in frames.
    #[arg(long, default_value_t = 1.5)]
    queue_limit: f64,
    /// Smallest frame sent while data is queued, as a share of a normal frame;
    /// below that the frame is skipped. 1.0 = skip instead of shrinking.
    #[arg(long, default_value_t = 0.25)]
    min_frame: f64,
    /// Outgoing datagram queue in KB; when full, the oldest datagrams are dropped.
    /// 0 = room for about 2.5 frames at --mbps.
    #[arg(long, default_value_t = 0)]
    send_buffer_kb: usize,
    /// Keep sending new frames even when earlier ones are still queued.
    #[arg(long)]
    no_skip: bool,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 60)]
    seconds: u64,
    /// Write per-second rows to this CSV file.
    #[arg(long)]
    csv: Option<PathBuf>,
}

#[derive(Default)]
struct Bucket {
    frame_rtt: Vec<f64>,
    input_rtt: Vec<f64>,
    acked_bytes: u64,
    sent: u64,
    skipped: u64,
    failed: u64,
    late: u64,
    recovered: u64,
    shards_expected: u64,
    shards_received: u64,
    target_bits: f64,
    parity_pct: u64,
    /// Frames sent smaller than the bitrate allows, and the sum of each sent
    /// frame's size as a percentage of a normal frame.
    shrunk: u64,
    size_pct: u64,
    /// Sum of the congestion controller's pacing rate in Mbps, one sample per second.
    cc_pacing: f64,
    cc_samples: u64,
}

impl Bucket {
    fn absorb(&mut self, o: &Bucket) {
        self.frame_rtt.extend_from_slice(&o.frame_rtt);
        self.input_rtt.extend_from_slice(&o.input_rtt);
        self.acked_bytes += o.acked_bytes;
        self.sent += o.sent;
        self.skipped += o.skipped;
        self.failed += o.failed;
        self.late += o.late;
        self.recovered += o.recovered;
        self.shards_expected += o.shards_expected;
        self.shards_received += o.shards_received;
        self.target_bits += o.target_bits;
        self.parity_pct += o.parity_pct;
        self.shrunk += o.shrunk;
        self.size_pct += o.size_pct;
        self.cc_pacing += o.cc_pacing;
        self.cc_samples += o.cc_samples;
    }

    fn size_pct(&self) -> f64 {
        if self.sent == 0 { 0.0 } else { self.size_pct as f64 / self.sent as f64 }
    }

    fn cc_pacing_mbps(&self) -> f64 {
        if self.cc_samples == 0 { f64::NAN } else { self.cc_pacing / self.cc_samples as f64 }
    }

    fn target_mbps(&self) -> f64 {
        if self.sent == 0 { 0.0 } else { self.target_bits / self.sent as f64 / 1e6 }
    }

    fn fec_pct(&self) -> f64 {
        if self.sent == 0 { 0.0 } else { self.parity_pct as f64 / self.sent as f64 }
    }
}

struct State {
    in_flight: HashMap<u32, (Instant, u64)>,
    min_rtt: f64,
    interval_ms: f64,
    rc: RateController,
    sec: Bucket,
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[((p / 100.0) * (s.len() - 1) as f64).round() as usize]
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 }
}

/// How the sender picks parity: a fixed percentage or following measured loss.
#[derive(Clone, Copy)]
enum Fec {
    Auto,
    Fixed(u32),
}

struct Sender {
    conn: Connection,
    st: Shared<State>,
    fps: u32,
    max_frame: usize,
    fixed_rate: Option<f64>,
    fec: Fec,
    pace: f64,
    send_buffer: usize,
    skip: bool,
    budget: Budget,
    dur: Duration,
}

pub async fn run(a: DialArgs) -> Result<()> {
    let fec = match a.fec.as_str() {
        "auto" => Fec::Auto,
        n => Fec::Fixed(n.parse().map_err(|_| anyhow!("--fec takes a number or \"auto\""))?),
    };
    let max_frame = (a.mbps * 1e6 / 8.0 / a.fps as f64) as usize;
    let send_buffer = match a.send_buffer_kb {
        0 => ((max_frame as f64 * (a.queue_limit + 0.5)) as usize).max(64 * 1024),
        kb => kb * 1024,
    };
    let relay: Option<RelayUrl> = if a.no_relay { None } else { Some(a.relay.parse()?) };
    let t_start = Instant::now();
    let cc_stats = Arc::new(crate::cc::CcStats::default());
    let transport = crate::transport_config(a.cc, Some(send_buffer), a.window_gain.max(1.0), cc_stats.clone());
    let ep = crate::endpoint(SecretKey::generate(), relay.clone(), !a.relay_only, transport).await?;
    let ips: &[SocketAddr] = if a.relay_only { &[] } else { &a.addr };
    let conn = ep
        .connect(crate::make_dial_addr(a.id, ips, relay.as_ref()), crate::ALPN)
        .await
        .map_err(|e| anyhow!("connect: {e:#}"))?;
    let connect_ms = elapsed_ms(t_start);
    let (k, addr, _) = crate::selected_path(&conn);
    println!("connected in {connect_ms:.0} ms via {k} {addr}");

    let t0 = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / a.fps as f64);
    let st: Shared<State> = Arc::new(Mutex::new(State {
        in_flight: HashMap::new(),
        min_rtt: f64::INFINITY,
        interval_ms: interval.as_secs_f64() * 1000.0,
        rc: RateController::new(RateConfig::new(a.min_mbps * 1e6, a.mbps * 1e6), t0),
        sec: Bucket::default(),
    }));
    spawn_feedback_reader(conn.clone(), st.clone());
    spawn_controller(conn.clone(), st.clone(), !a.fixed_rate);
    spawn_input(conn.clone(), st.clone(), t0).await?;
    let sender = Sender {
        conn: conn.clone(),
        st: st.clone(),
        fps: a.fps,
        max_frame,
        fixed_rate: a.fixed_rate.then_some(a.mbps * 1e6),
        fec,
        pace: a.pace.clamp(0.0, 0.95),
        send_buffer,
        skip: !a.no_skip,
        budget: Budget { queue_limit: a.queue_limit.max(1.0), min_share: a.min_frame.clamp(0.05, 1.0) },
        dur: Duration::from_secs(a.seconds),
    };
    let sender = tokio::spawn(sender.run());
    let mut csv = match &a.csv {
        Some(p) => Some(std::fs::File::create(p)?),
        None => None,
    };
    if let Some(f) = csv.as_mut() {
        writeln!(
            f,
            "t_s,path,addr,path_rtt_ms,frame_rtt_p50_ms,frame_rtt_p95_ms,failed_pct,late_pct,recovered_pct,shard_loss_pct,skipped_pct,target_mbps,mbps,fec_pct,input_rtt_p95_ms,cc_pacing_mbps,cwnd_kb"
        )?;
    }
    println!(
        "{:>4} {:<8} {:>6} {:>7} {:>7} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>5} {:>7} {:>6}",
        "t", "path", "rtt", "fr_p50", "fr_p95", "fail%", "late%", "fec%", "loss%", "skip%", "tgt", "Mbps", "par%", "in_p95", "ccpace"
    );
    let mut all = Bucket::default();
    let mut path_secs: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut first_direct = (k != "relay").then_some(connect_ms / 1000.0);
    let start = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut tick = tokio::time::interval_at(start, Duration::from_secs(1));
    for t in 1..=a.seconds {
        tick.tick().await;
        let mut b = std::mem::take(&mut st.lock().unwrap().sec);
        let ccpace = cc_stats.pacing_rate.load(std::sync::atomic::Ordering::Relaxed) as f64 * 8.0 / 1e6;
        let cwnd_kb = cc_stats.cwnd.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1024.0;
        if ccpace > 0.0 {
            b.cc_pacing += ccpace;
            b.cc_samples += 1;
        }
        let (k, addr, prtt) = crate::selected_path(&conn);
        if k != "relay" && k != "none" && first_direct.is_none() {
            first_direct = Some(elapsed_ms(t_start) / 1000.0);
        }
        *path_secs.entry(k).or_default() += 1;
        let (p50, p95) = (pct(&b.frame_rtt, 50.0), pct(&b.frame_rtt, 95.0));
        let acked = b.frame_rtt.len() as u64;
        let (fail, late) = (ratio(b.failed, b.sent), ratio(b.late, acked));
        let (rec, skip) = (ratio(b.recovered, acked), ratio(b.skipped, b.sent + b.skipped));
        let loss = ratio(b.shards_expected - b.shards_received.min(b.shards_expected), b.shards_expected);
        let (tgt, mbps, par) = (b.target_mbps(), b.acked_bytes as f64 * 8.0 / 1e6, b.fec_pct());
        let inp = pct(&b.input_rtt, 95.0);
        println!(
            "{t:>4} {k:<8} {prtt:>6.1} {p50:>7.1} {p95:>7.1} {fail:>6.1} {late:>6.1} {rec:>6.1} {loss:>6.1} {skip:>6.1} {tgt:>6.1} {mbps:>6.1} {par:>5.0} {inp:>7.1} {ccpace:>6.1}"
        );
        if let Some(f) = csv.as_mut() {
            writeln!(
                f,
                "{t},{k},{addr},{prtt:.2},{p50:.2},{p95:.2},{fail:.2},{late:.2},{rec:.2},{loss:.2},{skip:.2},{tgt:.2},{mbps:.2},{par:.1},{inp:.2},{ccpace:.2},{cwnd_kb:.1}"
            )?;
        }
        all.absorb(&b);
        if conn.close_reason().is_some() {
            println!("connection closed: {:?}", conn.close_reason());
            break;
        }
    }
    let _ = sender.await;
    tokio::time::sleep(FRAME_TIMEOUT + REPORT_EVERY * 3).await;
    {
        let mut s = st.lock().unwrap();
        let left = s.in_flight.len() as u64;
        s.in_flight.clear();
        s.sec.failed += left;
        all.absorb(&std::mem::take(&mut s.sec));
    }
    print_summary(&a, &all, connect_ms, first_direct, &path_secs);
    conn.close(0u32.into(), b"done");
    ep.close().await;
    Ok(())
}

fn print_summary(
    a: &DialArgs,
    all: &Bucket,
    connect_ms: f64,
    first_direct: Option<f64>,
    path_secs: &BTreeMap<&'static str, u64>,
) {
    let paths: Vec<String> = path_secs.iter().map(|(k, n)| format!("{k} {n}s")).collect();
    let acked = all.frame_rtt.len() as u64;
    let lost_shards = all.shards_expected - all.shards_received.min(all.shards_expected);
    println!("\n== summary ==");
    println!(
        "target            up to {:.1} Mbps, {} fps, {} s, fec {}, pace {}, cc {:?}, window gain {}, queue limit {} frames, min frame {}{}{}",
        a.mbps,
        a.fps,
        a.seconds,
        a.fec,
        a.pace,
        a.cc,
        a.window_gain,
        a.queue_limit,
        a.min_frame,
        if a.fixed_rate { ", fixed rate" } else { "" },
        if a.no_skip { ", no skip" } else { "" }
    );
    println!("connect time      {connect_ms:.0} ms");
    match first_direct {
        Some(s) => println!("first direct path {s:.1} s after start"),
        None => println!("first direct path never (relay only)"),
    }
    println!("time on path      {}", paths.join(", "));
    println!("frames sent       {}", all.sent);
    println!("frames skipped    {:.2} %  (sender had a backlog)", ratio(all.skipped, all.sent + all.skipped));
    println!(
        "frames shrunk     {:.2} %  (sent smaller because of a backlog), frame size {:.0} % of the bitrate's on average",
        ratio(all.shrunk, all.sent),
        all.size_pct()
    );
    println!("frames failed     {:.2} %  (not decodable within {} ms)", ratio(all.failed, all.sent), FRAME_TIMEOUT.as_millis());
    println!("frames recovered  {:.2} %  (needed parity shards)", ratio(all.recovered, acked));
    println!("frames late       {:.2} %  (rtt > best + 1 frame interval)", ratio(all.late, acked));
    println!("shard loss        {:.2} %", ratio(lost_shards, all.shards_expected));
    println!(
        "frame rtt ms      p50 {:.1}  p95 {:.1}  p99 {:.1}  min {:.1}",
        pct(&all.frame_rtt, 50.0),
        pct(&all.frame_rtt, 95.0),
        pct(&all.frame_rtt, 99.0),
        pct(&all.frame_rtt, 0.0)
    );
    println!("throughput        {:.1} Mbps", all.acked_bytes as f64 * 8.0 / 1e6 / a.seconds as f64);
    println!("bitrate chosen    {:.1} Mbps average, parity {:.0} %", all.target_mbps(), all.fec_pct());
    println!("input rtt ms      p50 {:.1}  p95 {:.1}", pct(&all.input_rtt, 50.0), pct(&all.input_rtt, 95.0));
    println!("cc pacing rate    {:.1} Mbps average", all.cc_pacing_mbps());
}

impl Sender {
    /// Simulated encoder output: one frame per tick at the controller's bitrate,
    /// cut into shards with parity and paced over part of the frame interval.
    async fn run(self) {
        let interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        let spread = interval.mul_f64(self.pace);
        let payload = vec![0u8; self.max_frame];
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let t0 = Instant::now();
        let mut frame: u32 = 0;
        while t0.elapsed() < self.dur && self.conn.close_reason().is_none() {
            tick.tick().await;
            let Some(max) = self.conn.max_datagram_size() else { break };
            let (bits, parity_pct) = {
                let s = self.st.lock().unwrap();
                let bits = self.fixed_rate.unwrap_or_else(|| s.rc.bitrate());
                let p = match self.fec {
                    Fec::Auto => s.rc.fec_pct(),
                    Fec::Fixed(p) => p,
                };
                (bits, p)
            };
            let normal = ((bits / 8.0 / self.fps as f64) as usize).clamp(1, self.max_frame);
            self.st.lock().unwrap().rc.on_frame_offered(normal as u64);
            let mut bytes = normal;
            if self.skip {
                // A real encoder would be asked for a smaller frame, or none at all:
                // earlier frames are still waiting to go out, and a full frame would
                // only queue behind them.
                let queued = self.send_buffer.saturating_sub(self.conn.datagram_send_buffer_space());
                match self.budget.decide(normal, queued) {
                    Frame::Skip => {
                        let mut s = self.st.lock().unwrap();
                        s.sec.skipped += 1;
                        s.rc.on_backlog();
                        continue;
                    }
                    Frame::Send(n) if n < normal => {
                        bytes = n;
                        let mut s = self.st.lock().unwrap();
                        s.sec.shrunk += 1;
                        s.rc.on_backlog();
                    }
                    Frame::Send(_) => {}
                }
            }
            let packets = encode_frame(frame, &payload[..bytes], max.min(1400), parity_pct, MIN_PARITY);
            {
                let mut s = self.st.lock().unwrap();
                s.in_flight.insert(frame, (Instant::now(), bytes as u64));
                s.sec.sent += 1;
                s.sec.target_bits += bits;
                s.sec.parity_pct += parity_pct as u64;
                s.sec.size_pct += (bytes * 100 / normal) as u64;
            }
            let plan = pacer::plan(packets.len(), spread, PACE_GAP);
            let start = tokio::time::Instant::now();
            let mut pkts = packets.into_iter().map(Bytes::from);
            for (off, n) in plan {
                if !off.is_zero() {
                    tokio::time::sleep_until(start + off).await;
                }
                for p in pkts.by_ref().take(n) {
                    let _ = self.conn.send_datagram(p);
                }
            }
            frame = frame.wrapping_add(1);
        }
    }
}

/// Reads frame acks (round trip, FEC use) and loss reports from the listener.
fn spawn_feedback_reader(conn: Connection, st: Shared<State>) {
    tokio::spawn(async move {
        while let Ok(d) = conn.read_datagram().await {
            let mut s = st.lock().unwrap();
            match d.first() {
                Some(&T_ACK) => {
                    let Some(ack) = Ack::parse(&d) else { continue };
                    let Some((sent_at, bytes)) = s.in_flight.remove(&ack.frame) else { continue };
                    let rtt = elapsed_ms(sent_at);
                    s.min_rtt = s.min_rtt.min(rtt);
                    if rtt > s.min_rtt + s.interval_ms {
                        s.sec.late += 1;
                    }
                    if ack.recovered {
                        s.sec.recovered += 1;
                    }
                    s.sec.frame_rtt.push(rtt);
                    s.sec.acked_bytes += bytes;
                    s.rc.on_frame_acked(rtt, bytes);
                }
                Some(&T_REPORT) => {
                    let Some(r) = Report::parse(&d) else { continue };
                    s.sec.shards_expected += r.shards_expected as u64;
                    s.sec.shards_received += r.shards_received as u64;
                    s.rc.on_report(&r);
                }
                _ => {}
            }
        }
    });
}

/// Every 100 ms: time out unacked frames and let the controller pick a new bitrate.
fn spawn_controller(conn: Connection, st: Shared<State>, adaptive: bool) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REPORT_EVERY);
        let mut last_path = "";
        while conn.close_reason().is_none() {
            tick.tick().await;
            let (k, _, _) = crate::selected_path(&conn);
            let now = Instant::now();
            let mut s = st.lock().unwrap();
            let before = s.in_flight.len();
            s.in_flight.retain(|_, (at, _)| now.duration_since(*at) < FRAME_TIMEOUT);
            let failed = before - s.in_flight.len();
            s.sec.failed += failed as u64;
            for _ in 0..failed {
                s.rc.on_frame_failed();
            }
            if k != last_path {
                s.rc.new_path();
                last_path = k;
            }
            if adaptive {
                s.rc.update(now);
            }
        }
    });
}

/// 200 small messages per second on a reliable stream, echoed back by the listener.
async fn spawn_input(conn: Connection, st: Shared<State>, t0: Instant) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| anyhow!("open_bi: {e:#}"))?;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        loop {
            tick.tick().await;
            let mut m = [0u8; INPUT_MSG];
            m[..8].copy_from_slice(&(t0.elapsed().as_nanos() as u64).to_be_bytes());
            if send.write_all(&m).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut m = [0u8; INPUT_MSG];
        while recv.read_exact(&mut m).await.is_ok() {
            let sent = u64::from_be_bytes(m[..8].try_into().unwrap());
            let rtt = (t0.elapsed().as_nanos() as u64).saturating_sub(sent) as f64 / 1e6;
            st.lock().unwrap().sec.input_rtt.push(rtt);
        }
    });
    Ok(())
}
