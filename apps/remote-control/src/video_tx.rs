//! Host side of the video stream: decides how big the next frame may be, then
//! cuts encoded frames into FEC shards and paces them out as datagrams.
//! Same logic as the phase-0 nettest sender, fed by a real encoder.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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

/// A frame not acked within this time counts as failed.
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);
pub const REPORT_EVERY: Duration = Duration::from_millis(100);
const PACE_GAP: Duration = Duration::from_millis(1);
const PACE_SHARE: f64 = 0.25;
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

pub struct VideoTx {
    conn: Connection,
    st: Mutex<State>,
    fps: u32,
    pacer: mpsc::UnboundedSender<Vec<Bytes>>,
    /// Encoded frames are dropped instead of sent (see `host::watch_path`).
    pub paused: AtomicBool,
}

impl VideoTx {
    /// Starts the pacing, feedback and rate-control tasks on the current runtime.
    pub fn start(conn: Connection, fps: u32, min_bps: f64, max_bps: f64) -> Arc<Self> {
        let (ptx, prx) = mpsc::unbounded_channel();
        let tx = Arc::new(VideoTx {
            conn: conn.clone(),
            st: Mutex::new(State {
                rc: RateController::new(RateConfig::new(min_bps, max_bps), Instant::now()),
                in_flight: HashMap::new(),
                frame: 0,
                sec: Second::default(),
                own_backlog_until: None,
            }),
            fps,
            pacer: ptx,
            paused: AtomicBool::new(false),
        });
        tokio::spawn(pace(conn, prx, Duration::from_secs_f64(PACE_SHARE / fps as f64)));
        tokio::spawn(tx.clone().read_feedback());
        tokio::spawn(tx.clone().control());
        tx
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
        let normal = ((bits / 8.0 / self.fps as f64) as usize).max(1);
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
        let (f, packets) = {
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
            (f, packets)
        };
        let _ = self.pacer.send(packets.into_iter().map(Bytes::from).collect());
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
            let (k, _, _) = net::selected_path(&self.conn);
            let now = Instant::now();
            let mut s = self.st.lock().unwrap();
            let before = s.in_flight.len();
            s.in_flight.retain(|_, (at, _)| now.duration_since(*at) < FRAME_TIMEOUT);
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

/// Spreads each frame's datagrams over a quarter of the frame interval.
async fn pace(conn: Connection, mut rx: mpsc::UnboundedReceiver<Vec<Bytes>>, spread: Duration) {
    while let Some(packets) = rx.recv().await {
        let plan = pacer::plan(packets.len(), spread, PACE_GAP);
        let start = tokio::time::Instant::now();
        let mut it = packets.into_iter();
        for (off, n) in plan {
            if !off.is_zero() {
                tokio::time::sleep_until(start + off).await;
            }
            for p in it.by_ref().take(n) {
                let _ = conn.send_datagram(p);
            }
        }
        if conn.close_reason().is_some() {
            return;
        }
    }
}
