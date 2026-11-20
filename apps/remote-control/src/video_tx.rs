//! Host side of the video stream: decides how big the next frame may be, then
//! cuts encoded frames into FEC shards and paces them out as datagrams.
//! Same logic as the phase-0 nettest sender, fed by a real encoder.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use iroh::endpoint::Connection;
use rc_stream::{
    budget::{Budget, Frame},
    fec::encode_frame,
    pacer,
    rate::{RateConfig, RateController},
    wire::{Ack, Report, T_ACK, T_REPORT},
};
use tokio::sync::mpsc;

use crate::{
    cc,
    net::{self, SEND_BUFFER},
    proto::HostStats,
    stats::percentile,
};

/// A frame not acked within this time, or within twice the path's round trip
/// if that is longer, counts as failed. With a fixed 500 ms, every frame on a
/// relay at 0.7-2 s round trip failed and its late ack was not counted as
/// delivered, so the rate could only fall.
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);
pub const REPORT_EVERY: Duration = Duration::from_millis(100);
const PACE_GAP: Duration = Duration::from_millis(1);
const PACE_SHARE: f64 = 0.5;
const PACE_MAX: Duration = Duration::from_millis(5);
const MIN_PARITY: u16 = 2;
const MAX_DATAGRAM: usize = 1400;

pub enum Plan {
    Skip,
    /// Encode at `bitrate` bits/s. `chosen` is the rate controller's bitrate,
    /// `normal` the frame size the encoder's rate gives, and `shrunk` means
    /// `bitrate` is lowered for this frame only.
    Encode { chosen: u32, bitrate: u32, normal: usize, shrunk: bool },
}

/// Shrinking frames: while up to 1.25 frames are queued, ask the encoder for
/// a smaller frame instead of skipping. Off by default (host `--shrink`):
/// neither NVIDIA's nor Intel's Media Foundation encoder makes a single frame
/// smaller when the bitrate is lowered for that frame only (enc-test,
/// 2026-11-06), so the shrunk frame came out full size and only lengthened the queue.
const SHRINKING: Budget = Budget { queue_limit: 1.5, min_share: 0.25 };
/// Whole frames: send while at most one frame is queued, otherwise skip.
const WHOLE_FRAMES: Budget = Budget { queue_limit: 2.0, min_share: 1.0 };

#[derive(Default)]
struct Second {
    sent: u64,
    skipped: u64,
    shrunk: u64,
    bytes: u64,
    frame_rtt: Vec<f64>,
    host_us: Vec<f64>,
    encode_us: Vec<f64>,
    /// Encoded size over the size asked for, per frame.
    size_ratio: Vec<f64>,
    /// Largest datagram queue seen when planning a frame, bytes.
    queued_max: usize,
    /// Frames over three times the size asked for: (bytes, budget, key).
    big: Vec<(usize, usize, bool)>,
}

struct State {
    rc: RateController,
    in_flight: HashMap<u32, (Instant, u64)>,
    frame: u32,
    sec: Second,
    /// Until then the queue is explained by a frame we sent well over its
    /// budget (a key frame, the first frame after a still screen): frames
    /// skipped or shrunk meanwhile say nothing about the link.
    own_backlog_until: Option<Instant>,
}

/// A frame queued for the pacer task.
struct PaceFrame {
    packets: Vec<Bytes>,
    bytes: usize,
    link_bps: f64,
    /// True when the path is a low-RTT direct LAN link (<10 ms) where QUIC's
    /// internal microsecond BBR pacer handles normal frames with lower latency
    /// than coarse 1 ms application-level sleeps.
    fast_lan: bool,
}

pub struct VideoTx {
    conn: Connection,
    st: Mutex<State>,
    /// Frames per second the encoder runs at now (see `configure`).
    fps: Arc<AtomicU32>,
    pacer: mpsc::UnboundedSender<PaceFrame>,
    /// Set by `host` whenever a control message is written so the datagram
    /// pacer briefly lets `datagrams.outgoing` drain (in `noq-proto`,
    /// DATAGRAM frames strictly preempt STREAM frames).
    ctrl_pending: Arc<AtomicBool>,
    /// Encoded frames are dropped instead of sent (see `host::watch_path`).
    pub paused: AtomicBool,
}

/// Lowest bitrate the rate controller goes down to (when the highest allows).
const MIN_BPS: f64 = 1.5e6;

impl VideoTx {
    /// Starts the pacing, feedback and rate-control tasks on the current runtime.
    pub fn start(conn: Connection, fps: u32, max_bps: f64) -> Arc<Self> {
        let (ptx, prx) = mpsc::unbounded_channel();
        let fps = Arc::new(AtomicU32::new(fps.max(1)));
        let ctrl_pending = Arc::new(AtomicBool::new(false));
        let tx = Arc::new(VideoTx {
            conn: conn.clone(),
            st: Mutex::new(State {
                rc: RateController::new(RateConfig::new(MIN_BPS.min(max_bps), max_bps), Instant::now()),
                in_flight: HashMap::new(),
                frame: 0,
                sec: Second::default(),
                own_backlog_until: None,
            }),
            fps: fps.clone(),
            pacer: ptx,
            ctrl_pending: ctrl_pending.clone(),
            paused: AtomicBool::new(false),
        });
        tokio::spawn(pace(conn, prx, fps, ctrl_pending));
        tokio::spawn(tx.clone().read_feedback());
        tokio::spawn(tx.clone().control());
        tx
    }

    /// Signals that a control stream message is queued and should not be
    /// starved behind continuous video datagrams.
    pub fn notify_ctrl(&self) {
        self.ctrl_pending.store(true, Ordering::Release);
    }

    /// The picture the capture loop streams from now on: its frame rate and
    /// highest bitrate. Before the first frame the rate starts at the highest.
    pub fn configure(&self, fps: u32, max_bps: f64) {
        self.fps.store(fps.max(1), Ordering::Relaxed);
        let mut s = self.st.lock().unwrap();
        if s.frame == 0 {
            s.rc = RateController::new(RateConfig::new(MIN_BPS.min(max_bps), max_bps), Instant::now());
        } else {
            s.rc.set_max(max_bps);
        }
    }

    /// Call when a new screen image is ready, before encoding it.
    /// `encoder_bps`: the bitrate the encoder runs at now; frame sizes are
    /// measured against it, since an encoder that changes its rate only
    /// rarely (`Encoder::rekeys_on_rate_change`) may lag the chosen rate.
    /// `shrink`: see `SHRINKING`.
    pub fn plan(&self, encoder_bps: u32, shrink: bool) -> Plan {
        let mut s = self.st.lock().unwrap();
        let chosen = s.rc.bitrate();
        let bits = encoder_bps as f64;
        let normal = ((bits / 8.0 / self.fps.load(Ordering::Relaxed) as f64) as usize).max(1);
        let queued = SEND_BUFFER.saturating_sub(self.conn.datagram_send_buffer_space());
        s.sec.queued_max = s.sec.queued_max.max(queued);
        let link_signal = s.own_backlog_until.is_none_or(|t| Instant::now() >= t);
        let budget = if shrink { SHRINKING } else { WHOLE_FRAMES };
        match budget.decide(normal, queued) {
            Frame::Skip => {
                s.sec.skipped += 1;
                if link_signal {
                    s.rc.on_backlog();
                    s.rc.on_frame_offered(normal as u64);
                }
                Plan::Skip
            }
            Frame::Send(n) if n < normal => {
                s.sec.shrunk += 1;
                if link_signal {
                    s.rc.on_backlog();
                }
                let bitrate = (bits * n as f64 / normal as f64) as u32;
                Plan::Encode { chosen: chosen as u32, bitrate, normal, shrunk: true }
            }
            Frame::Send(_) => Plan::Encode { chosen: chosen as u32, bitrate: bits as u32, normal, shrunk: false },
        }
    }

    /// Hands an encoded frame (header included) to the network. `budget` is
    /// the size the encoder was asked for (after any shrinking). Returns the
    /// frame's number on the wire.
    pub fn submit(&self, payload: &[u8], normal: usize, budget: usize, shrunk: bool, host_us: u32, encode_us: u32) -> u32 {
        if self.paused.load(Ordering::Relaxed) {
            return 0;
        }
        let max = self.conn.max_datagram_size().unwrap_or(1200).min(MAX_DATAGRAM);
        let (f, packets, rate_bps) = {
            let mut s = self.st.lock().unwrap();
            let f = s.frame;
            s.frame = s.frame.wrapping_add(1);
            s.rc.on_frame_offered(if shrunk { normal } else { payload.len() } as u64);
            if payload.len() > 2 * budget {
                // Time the extra bytes need at the chosen rate.
                let extra = (payload.len() - budget) as f64 * 8.0 / s.rc.bitrate().max(1.0);
                let until = Instant::now() + Duration::from_secs_f64(extra);
                s.own_backlog_until = Some(s.own_backlog_until.map_or(until, |t| t.max(until)));
            }
            let packets = encode_frame(f, payload, max, s.rc.fec_pct(), MIN_PARITY);
            s.in_flight.insert(f, (Instant::now(), payload.len() as u64));
            s.sec.sent += 1;
            s.sec.bytes += payload.len() as u64;
            s.sec.host_us.push(host_us as f64);
            s.sec.encode_us.push(encode_us as f64);
            s.sec.size_ratio.push(payload.len() as f64 / budget.max(1) as f64);
            if payload.len() > 3 * budget {
                let key = crate::proto::FrameHeader::parse(payload).is_some_and(|(h, _)| h.key);
                s.sec.big.push((payload.len(), budget, key));
            }
            (f, packets, s.rc.bitrate())
        };
        let bytes: usize = packets.iter().map(Vec::len).sum();
        let link_bps = net::path_congestion(&self.conn)
            .and_then(|(_, bw, _, _)| bw)
            .filter(|&bw| bw > 0)
            .map_or(rate_bps, |bw| bw as f64);
        let (path_kind, _, path_rtt) = net::selected_path(&self.conn);
        let fast_lan = path_kind == net::PathKind::Direct && path_rtt < 10.0;
        let _ = self.pacer.send(PaceFrame {
            packets: packets.into_iter().map(Bytes::from).collect(),
            bytes,
            link_bps,
            fast_lan,
        });
        f
    }

    /// Counters for the last second; resets them. Also writes one line to the
    /// host's log, for diagnosing the sending side.
    pub fn take_stats(&self) -> HostStats {
        let (sec, rate) = {
            let mut s = self.st.lock().unwrap();
            (std::mem::take(&mut s.sec), s.rc.bitrate())
        };
        let (cwnd, bw, lost, events) = net::path_congestion(&self.conn).unwrap_or_default();
        let big: Vec<String> = sec
            .big
            .iter()
            .map(|(b, budget, key)| format!("{}{}/{} KB", if *key { "key " } else { "" }, b / 1000, budget / 1000))
            .collect();
        crate::log!(
            "tx {} sent {} skipped {} shrunk, rate {:.1} Mbps, queue max {} KB, window {} KB, BBR estimate {:.1} Mbps, lost pkts {lost}, cc events {events}{}",
            sec.sent,
            sec.skipped,
            sec.shrunk,
            rate / 1e6,
            sec.queued_max / 1000,
            cwnd / 1000,
            bw.unwrap_or(0) as f64 / 1e6,
            if big.is_empty() { String::new() } else { format!(", big frames: {}", big.join(", ")) },
        );
        let (path, _, rtt) = net::selected_path(&self.conn);
        let due = (sec.sent + sec.skipped).max(1);
        HostStats {
            fps: sec.sent as u16,
            kbps: (sec.bytes * 8 / 1000) as u32,
            skipped_pm: (sec.skipped * 1000 / due) as u16,
            shrunk_pm: (sec.shrunk * 1000 / due) as u16,
            frame_rtt_p50_us: (percentile(&sec.frame_rtt, 50.0) * 1000.0) as u32,
            frame_rtt_p95_us: (percentile(&sec.frame_rtt, 95.0) * 1000.0) as u32,
            host_p50_us: percentile(&sec.host_us, 50.0) as u32,
            host_p95_us: percentile(&sec.host_us, 95.0) as u32,
            encode_p50_us: percentile(&sec.encode_us, 50.0) as u32,
            encode_p95_us: percentile(&sec.encode_us, 95.0) as u32,
            path: path.code(),
            path_rtt_us: (rtt * 1000.0) as u32,
            rate_kbps: (rate / 1000.0) as u32,
            size_p50_pct: pct(percentile(&sec.size_ratio, 50.0)),
            size_max_pct: pct(sec.size_ratio.iter().copied().fold(0.0, f64::max)),
        }
    }

    async fn read_feedback(self: Arc<Self>) {
        while let Ok(d) = self.conn.read_datagram().await {
            let mut s = self.st.lock().unwrap();
            match d.first() {
                Some(&T_ACK) => {
                    let Some(ack) = Ack::parse(&d) else { continue };
                    let Some((sent_at, bytes)) = s.in_flight.remove(&ack.frame) else { continue };
                    let ms = sent_at.elapsed().as_secs_f64() * 1000.0;
                    s.sec.frame_rtt.push(ms);
                    s.rc.on_frame_acked(ms, bytes);
                }
                Some(&T_REPORT) => {
                    if let Some(r) = Report::parse(&d) {
                        s.rc.on_report(&r);
                    }
                }
                _ => {}
            }
        }
    }

    /// Every 100 ms: time out unacked frames, pick a new bitrate and give it
    /// to the congestion controller as its floor.
    async fn control(self: Arc<Self>) {
        let mut tick = tokio::time::interval(REPORT_EVERY);
        let mut last_path = net::PathKind::None;
        while self.conn.close_reason().is_none() {
            tick.tick().await;
            let (k, _, rtt) = net::selected_path(&self.conn);
            let timeout = FRAME_TIMEOUT.max(Duration::from_secs_f64(rtt.max(0.0) * 2.0 / 1000.0));
            let now = Instant::now();
            let mut s = self.st.lock().unwrap();
            let before = s.in_flight.len();
            s.in_flight.retain(|_, (at, _)| now.duration_since(*at) < timeout);
            for _ in 0..before - s.in_flight.len() {
                s.rc.on_frame_failed();
            }
            if k != last_path {
                s.rc.reset_base();
                last_path = k;
            }
            let rate = s.rc.update(now);
            cc::set_floor(rate, s.rc.fec_pct());
        }
        cc::clear_floor();
    }
}

/// Ratio to percent for the stats message; 0 when there were no frames.
fn pct(ratio: f64) -> u16 {
    if ratio.is_nan() { 0 } else { (ratio * 100.0).min(65535.0) as u16 }
}

/// Adaptive pacing spread: burst small frames and normal frames on a fast
/// direct LAN immediately (QUIC's built-in microsecond BBR pacer handles them
/// without coarse 1 ms sleeps), and spread giant keyframes (>64 KB) or higher-
/// RTT Wi-Fi/WAN frames at ~1.25x the measured BBR link bandwidth (capped at
/// half a frame interval and `PACE_MAX`).
fn pace_spread(packets: usize, bytes: usize, fps: u32, link_bps: f64, fast_lan: bool) -> Duration {
    if packets <= 6 || (fast_lan && bytes <= 64_000) {
        return Duration::ZERO;
    }
    let max_spread = Duration::from_secs_f64(PACE_SHARE / fps.max(1) as f64).min(PACE_MAX);
    let adaptive = Duration::from_secs_f64((bytes as f64 * 8.0) / (link_bps.max(1.0) * 1.25)).min(max_spread);
    if adaptive < PACE_GAP { Duration::ZERO } else { adaptive }
}

/// Spreads each frame's datagrams smoothly when needed, without adding fixed
/// delay to small frames on fast links or starving control stream messages.
async fn pace(
    conn: Connection,
    mut rx: mpsc::UnboundedReceiver<PaceFrame>,
    fps: Arc<AtomicU32>,
    ctrl_pending: Arc<AtomicBool>,
) {
    while let Some(frame) = rx.recv().await {
        let spread = pace_spread(frame.packets.len(), frame.bytes, fps.load(Ordering::Relaxed), frame.link_bps, frame.fast_lan);
        let plan = pacer::plan(frame.packets.len(), spread, PACE_GAP);
        let start = tokio::time::Instant::now();
        let mut it = frame.packets.into_iter();
        for (off, n) in plan {
            if !off.is_zero() {
                tokio::time::sleep_until(start + off).await;
            }
            for p in it.by_ref().take(n) {
                if ctrl_pending.swap(false, Ordering::AcqRel) {
                    // `noq-proto` packs DATAGRAM frames ahead of STREAM frames;
                    // let `datagrams.outgoing` drain briefly so control messages
                    // (Pong, Stats, Cursor) never starve during high-bitrate motion.
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(2);
                    while conn.datagram_send_buffer_space() < SEND_BUFFER && tokio::time::Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_micros(250)).await;
                    }
                    tokio::task::yield_now().await;
                }
                let _ = conn.send_datagram(p);
            }
        }
        if conn.close_reason().is_some() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_spread_is_fast_on_lan_and_smooth_on_wifi_and_wan() {
        // Small frame: no pacing delay regardless of link speed.
        assert_eq!(pace_spread(5, 6_000, 60, 2e6, false), Duration::ZERO);
        // Fast direct LAN (<10 ms RTT), normal 30 KB drag frame: immediate (QUIC BBR paces it).
        assert_eq!(pace_spread(24, 30_000, 120, 60e6, true), Duration::ZERO);
        // Fast direct LAN, giant 180 KB keyframe: still smoothly spread (capped at 0.5/120).
        let lan_key = pace_spread(140, 180_000, 120, 80e6, true);
        assert!((lan_key.as_secs_f64() - (PACE_SHARE / 120.0)).abs() < 1e-6);
        // Higher-RTT Wi-Fi/WAN (60 Mbps BBR, 120 fps), 20 KB drag frame: ~2.13 ms.
        let wifi = pace_spread(16, 20_000, 120, 60e6, false);
        assert!(wifi >= Duration::from_micros(2000) && wifi <= Duration::from_micros(2300));
        // Higher-RTT Wi-Fi/WAN, 45 KB frame: capped at 0.5 / 120 (~4.17 ms).
        let wifi_big = pace_spread(36, 45_000, 120, 60e6, false);
        assert!((wifi_big.as_secs_f64() - (PACE_SHARE / 120.0)).abs() < 1e-6);
        // Slow WAN (4 Mbps, 60 fps), 20 KB frame: capped at PACE_MAX (5 ms).
        assert_eq!(pace_spread(16, 20_000, 60, 4e6, false), PACE_MAX);
    }
}
