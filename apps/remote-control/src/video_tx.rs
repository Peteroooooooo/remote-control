//! Host side of the video stream: decides how big the next frame may be, then
//! cuts encoded frames into FEC shards and paces them out as datagrams.
//! Same logic as the phase-0 nettest sender, fed by a real encoder.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use iroh::endpoint::Connection;
use rc_stream::{
    budget::{Budget, Frame},
    fec::{FrameEncoder, parity_count},
    pacer,
    rate::{RateConfig, RateController},
    rate::PROBE_ABORT_MS,
    wire::{self, Ack, ProbeResult, Report, Resend, SHARD_HDR, ShardHeader, T_ACK, T_PROBE_RESULT, T_REPORT, T_RESEND, probe_datagram},
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
/// But never more than this: a round trip that our own queue drove to 16 s
/// (phone hotspot, 2026-11-23) had frames wait 32 s before they failed.
const MAX_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
pub const REPORT_EVERY: Duration = Duration::from_millis(100);
const PACE_GAP: Duration = Duration::from_millis(1);
const PACE_SHARE: f64 = 0.5;
const PACE_MAX: Duration = Duration::from_millis(5);

const MAX_DATAGRAM: usize = 1400;
/// How long a probe (see `rc_stream::rate`) sends padding, and how often it
/// tops it up. Frames skipped until `PROBE_QUIET` after it are its doing,
/// not the link's: the padding fills the link on purpose.
const PROBE_TIME: Duration = Duration::from_millis(200);
const PROBE_STEP: Duration = Duration::from_millis(5);
const PROBE_QUIET: Duration = Duration::from_millis(300);

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
    /// Resend requests answered, and shards sent again for them.
    resend_requests: u64,
    resent_shards: u64,
}

/// Datagrams of the frames sent lately, for resend requests: no older than
/// `RESEND_KEEP` and no more than `RESEND_KEEP_BYTES` in all.
#[derive(Default)]
struct Sent {
    frames: VecDeque<(u32, Instant, Vec<Bytes>)>,
    bytes: usize,
}

const RESEND_KEEP: Duration = Duration::from_secs(2);
const RESEND_KEEP_BYTES: usize = 16 << 20;

impl Sent {
    fn add(&mut self, frame: u32, now: Instant, packets: Vec<Bytes>) {
        self.bytes += packets.iter().map(Bytes::len).sum::<usize>();
        self.frames.push_back((frame, now, packets));
        while let Some((_, at, p)) = self.frames.front()
            && (now.duration_since(*at) > RESEND_KEEP || self.bytes > RESEND_KEEP_BYTES)
        {
            self.bytes -= p.iter().map(Bytes::len).sum::<usize>();
            self.frames.pop_front();
        }
    }

    /// The datagrams `r` asks for, marked as sent again.
    fn wanted(&self, r: &Resend) -> Vec<Bytes> {
        let Some((_, _, packets)) = self.frames.iter().rev().find(|(f, _, _)| *f == r.frame) else {
            return Vec::new();
        };
        let pick = |p: &Bytes| Bytes::from(wire::resent_shard(p));
        if r.shards.is_empty() {
            let data = packets.first().and_then(|p| ShardHeader::parse(p)).map_or(0, |(h, _)| h.data as usize);
            packets.iter().take(data).map(pick).collect()
        } else {
            r.shards.iter().filter_map(|&i| packets.get(i as usize)).map(pick).collect()
        }
    }
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
    /// Recent frames' size over the size the rate gives them; a skipped frame
    /// is offered at that share. Frames are 35-60% of it on a desktop (phone
    /// hotspot 2026-11-24), and a skipped frame offered at the full size made
    /// the link look half as fast as it was.
    typical: f64,
    /// Most recent keyframe number sent, used to avoid endlessly re-requesting
    /// a keyframe on a still screen if reverse ACKs are lost.
    last_key_frame: Option<u32>,
    /// Last time a timed-out tail frame triggered an automatic keyframe.
    last_timeout_rekey: Option<Instant>,
    /// Frame datagram bytes handed to the pacer so far, and the rate they
    /// went at lately (bits/s, over a few updates).
    wire_sent: u64,
    wire_seen: u64,
    wire_bps: f64,
    /// Probe padding bytes handed to the connection so far.
    padding_sent: u64,
    /// Bytes handed over (`wire_sent` + `padding_sent`) once the latest key
    /// frame was: until that many have left our queues, it has not either.
    key_end: Option<u64>,
    /// The probe running: its rate, until when the transport's floor
    /// follows it.
    probe_floor: Option<(f64, Instant)>,
    /// The last probe sent: its number, the rate that went out while it
    /// ran (frames and padding, bits/s) and whether our own queue held some
    /// padding back.
    probe_sent: Option<(u16, f64, bool)>,
    /// The probe whose padding is still going out, and the viewer's answer
    /// if it came before the padding ended: the viewer answers once no
    /// padding came for `PROBE_DONE` (video_rx), which padding held back by
    /// our own queue can be. Such an answer waits for the probe to end.
    probe_running: Option<u16>,
    probe_early: Option<ProbeResult>,
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
    /// Reusable Reed-Solomon encoder; kept outside `st` so feedback/control
    /// tasks never block behind FEC encoding.
    encoder: Mutex<FrameEncoder>,
    sent: Mutex<Sent>,
    /// Frames per second the encoder runs at now (see `configure`).
    fps: Arc<AtomicU32>,
    pacer: mpsc::UnboundedSender<PaceFrame>,
    /// Total datagram bytes currently queued inside `pacer` awaiting send.
    pacer_bytes: Arc<AtomicUsize>,
    /// Shared flag telling the capture pipeline to emit a keyframe.
    want_key: Arc<AtomicBool>,
    /// Set by `host` whenever a control message is written so the datagram
    /// pacer briefly lets `datagrams.outgoing` drain (in `noq-proto`,
    /// DATAGRAM frames strictly preempt STREAM frames).
    ctrl_pending: Arc<AtomicBool>,
    /// Encoded frames are dropped instead of sent (see `host::watch_path`).
    pub paused: AtomicBool,
}

/// Lowest bitrate the rate controller goes down to (when the highest allows).
const MIN_BPS: f64 = 1.5e6;
/// Least share of the rate's frame size a skipped frame is offered at: after
/// a still screen `typical` is tiny, and the frames skipped when motion
/// starts must still show that the link falls short.
const MIN_TYPICAL: f64 = 0.5;

impl VideoTx {
    /// Starts the pacing, feedback and rate-control tasks on the current runtime.
    pub fn start(conn: Connection, fps: u32, max_bps: f64, want_key: Arc<AtomicBool>) -> Arc<Self> {
        let (ptx, prx) = mpsc::unbounded_channel();
        let fps = Arc::new(AtomicU32::new(fps.max(1)));
        let pacer_bytes = Arc::new(AtomicUsize::new(0));
        let ctrl_pending = Arc::new(AtomicBool::new(false));
        let tx = Arc::new(VideoTx {
            conn: conn.clone(),
            st: Mutex::new(State {
                rc: RateController::new(RateConfig::new(MIN_BPS.min(max_bps), max_bps), Instant::now()),
                in_flight: HashMap::new(),
                frame: 0,
                sec: Second::default(),
                own_backlog_until: None,
                typical: 1.0,
                last_key_frame: None,
                last_timeout_rekey: None,
                wire_sent: 0,
                wire_seen: 0,
                wire_bps: 0.0,
                padding_sent: 0,
                key_end: None,
                probe_floor: None,
                probe_sent: None,
                probe_running: None,
                probe_early: None,
            }),
            encoder: Mutex::new(FrameEncoder::new()),
            sent: Mutex::new(Sent::default()),
            fps: fps.clone(),
            pacer: ptx,
            pacer_bytes: pacer_bytes.clone(),
            want_key,
            ctrl_pending: ctrl_pending.clone(),
            paused: AtomicBool::new(false),
        });
        tokio::spawn(pace(conn, prx, fps, pacer_bytes, ctrl_pending));
        tokio::spawn(tx.clone().read_feedback());
        tokio::spawn(tx.clone().control());
        tx
    }

    /// True while video transmission is paused (e.g. only a public relay is up).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
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

    /// True while some of the latest key frame still waits in our own
    /// queues: a key frame asked for now was asked for before that one could
    /// arrive. On a 1.5 Mbps link a 140 KB key frame takes 0.75 s, the
    /// viewer asked again every 300 ms meanwhile, and a second key frame
    /// made the picture stand still 1.8 s (2026-11-24).
    pub fn key_frame_queued(&self) -> bool {
        let queued = SEND_BUFFER.saturating_sub(self.conn.datagram_send_buffer_space()) + self.pacer_bytes.load(Ordering::Relaxed);
        let s = self.st.lock().unwrap();
        s.key_end.is_some_and(|end| (s.wire_sent + s.padding_sent).saturating_sub(queued as u64) < end)
    }

    /// The rate controller's bitrate now, bits/s.
    pub fn bitrate(&self) -> f64 {
        self.st.lock().unwrap().rc.bitrate()
    }

    /// Call when a new screen image is ready, before encoding it.
    /// `encoder_bps`: the bitrate the encoder runs at now; frame sizes are
    /// measured against it, since an encoder that changes its rate only
    /// rarely (`Encoder::rekeys_on_rate_change`) may lag the chosen rate.
    /// `shrink`: see `SHRINKING`.
    pub fn plan(&self, encoder_bps: u32, shrink: bool) -> Plan {
        let max_datagram = self.conn.max_datagram_size().unwrap_or(1200).min(MAX_DATAGRAM);
        let mut s = self.st.lock().unwrap();
        let chosen = s.rc.bitrate();
        let bits = encoder_bps as f64;
        let normal = ((bits / 8.0 / self.fps.load(Ordering::Relaxed) as f64) as usize).max(1);
        let wire_normal = wire_frame_bytes(normal, s.rc.fec_pct(), max_datagram, s.rc.min_parity());
        let queued = SEND_BUFFER.saturating_sub(self.conn.datagram_send_buffer_space())
            + self.pacer_bytes.load(Ordering::Relaxed);
        s.sec.queued_max = s.sec.queued_max.max(queued);
        let link_signal = s.own_backlog_until.is_none_or(|t| Instant::now() >= t);
        let budget = if shrink { SHRINKING } else { WHOLE_FRAMES };
        match budget.decide(wire_normal, queued) {
            Frame::Skip => {
                s.sec.skipped += 1;
                if link_signal {
                    s.rc.on_backlog();
                    let typical = (normal as f64 * s.typical.clamp(MIN_TYPICAL, 1.5)) as u64;
                    s.rc.on_frame_offered(typical);
                }
                Plan::Skip
            }
            Frame::Send(n) if n < wire_normal => {
                s.sec.shrunk += 1;
                if link_signal {
                    s.rc.on_backlog();
                }
                let bitrate = (bits * n as f64 / wire_normal as f64) as u32;
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
        let (f, fec_pct, min_parity, rate_bps, is_key) = {
            let mut s = self.st.lock().unwrap();
            let f = s.frame;
            s.frame = s.frame.wrapping_add(1);
            s.rc.on_frame_offered(if shrunk { normal } else { payload.len() } as u64);
            let is_key = crate::proto::FrameHeader::parse(payload).is_some_and(|(h, _)| h.key);
            if payload.len() > 2 * budget {
                // Time the extra bytes need at the chosen rate.
                let extra = (payload.len() - budget) as f64 * 8.0 / s.rc.bitrate().max(1.0);
                let until = Instant::now() + Duration::from_secs_f64(extra);
                s.own_backlog_until = Some(s.own_backlog_until.map_or(until, |t| t.max(until)));
            } else if !shrunk && !is_key {
                s.typical = 0.9 * s.typical + 0.1 * payload.len() as f64 / normal.max(1) as f64;
            }
            if is_key {
                s.last_key_frame = Some(f);
            }
            s.in_flight.insert(f, (Instant::now(), payload.len() as u64));
            s.sec.sent += 1;
            s.sec.bytes += payload.len() as u64;
            s.sec.host_us.push(host_us as f64);
            s.sec.encode_us.push(encode_us as f64);
            s.sec.size_ratio.push(payload.len() as f64 / budget.max(1) as f64);
            if payload.len() > 3 * budget {
                s.sec.big.push((payload.len(), budget, is_key));
            }
            (f, s.rc.fec_pct(), s.rc.min_parity(), s.rc.bitrate(), is_key)
        };
        let packets: Vec<Bytes> =
            self.encoder.lock().unwrap().encode_frame(f, payload, max, fec_pct, min_parity).into_iter().map(Bytes::from).collect();
        let bytes: usize = packets.iter().map(Bytes::len).sum();
        self.sent.lock().unwrap().add(f, Instant::now(), packets.clone());
        {
            let mut s = self.st.lock().unwrap();
            s.wire_sent += bytes as u64;
            if is_key {
                s.key_end = Some(s.wire_sent + s.padding_sent);
            }
        }
        let link_bps = net::path_congestion(&self.conn)
            .and_then(|(_, bw, _, _)| bw)
            .filter(|&bw| bw > 0)
            .map_or(rate_bps, |bw| bw as f64);
        let (path_kind, _, path_rtt) = net::selected_path(&self.conn);
        let fast_lan = path_kind == net::PathKind::Direct && path_rtt < 10.0;
        self.pacer_bytes.fetch_add(bytes, Ordering::Relaxed);
        if self
            .pacer
            .send(PaceFrame {
                packets,
                bytes,
                link_bps,
                fast_lan,
            })
            .is_err()
        {
            self.pacer_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
        f
    }

    /// Counters for the last second; resets them. Also writes one line to the
    /// host's log, for diagnosing the sending side.
    pub fn take_stats(&self) -> HostStats {
        let (sec, rate, link) = {
            let mut s = self.st.lock().unwrap();
            (std::mem::take(&mut s.sec), s.rc.bitrate(), s.rc.link())
        };
        let (cwnd, bw, lost, events) = net::path_congestion(&self.conn).unwrap_or_default();
        let big: Vec<String> = sec
            .big
            .iter()
            .map(|(b, budget, key)| format!("{}{}/{} KB", if *key { "key " } else { "" }, b / 1000, budget / 1000))
            .collect();
        crate::log!(
            "tx {} sent {} skipped {} shrunk, rate {:.1} Mbps, link {}, queue max {} KB, window {} KB, BBR estimate {:.1} Mbps, lost pkts {lost}, cc events {events}{}{}",
            sec.sent,
            sec.skipped,
            sec.shrunk,
            rate / 1e6,
            link.map_or("?".to_string(), |l| format!("{:.1}", l / 1e6)),
            sec.queued_max / 1000,
            cwnd / 1000,
            bw.unwrap_or(0) as f64 / 1e6,
            if sec.resend_requests == 0 {
                String::new()
            } else {
                format!(", {} shards sent again for {} requests", sec.resent_shards, sec.resend_requests)
            },
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
                    if let Some((sent_at, bytes)) = s.in_flight.remove(&ack.frame) {
                        if ack.resent {
                            // Its delay is the resend's round trip, not the path's.
                            s.rc.on_frame_acked_late(bytes.min(u32::MAX as u64) as u32);
                        } else {
                            let ms = sent_at.elapsed().as_secs_f64() * 1000.0;
                            s.sec.frame_rtt.push(ms);
                            s.rc.on_frame_acked(ms, bytes);
                        }
                    }
                    // Redundant ACK bitmap for the preceding 32 frames: credit
                    // delivered bytes and prevent false timeouts without
                    // polluting RTT samples with later frames' arrival times.
                    let mut mask = ack.prev_mask;
                    while mask != 0 {
                        let bit = mask.trailing_zeros();
                        mask &= mask - 1;
                        let prev = ack.frame.wrapping_sub(bit + 1);
                        if let Some((_, bytes)) = s.in_flight.remove(&prev) {
                            s.rc.on_frame_acked_late(bytes.min(u32::MAX as u64) as u32);
                        }
                    }
                }
                Some(&T_REPORT) => {
                    if let Some(r) = Report::parse(&d) {
                        s.rc.on_report(&r);
                    }
                }
                Some(&T_RESEND) => {
                    let Some(r) = Resend::parse(&d) else { continue };
                    let again = self.sent.lock().unwrap().wanted(&r);
                    s.sec.resend_requests += 1;
                    s.sec.resent_shards += again.len() as u64;
                    for p in again {
                        s.wire_sent += p.len() as u64;
                        if self.conn.send_datagram(p).is_err() {
                            break;
                        }
                    }
                }
                Some(&T_PROBE_RESULT) => {
                    let Some(r) = ProbeResult::parse(&d) else { continue };
                    if s.probe_sent.is_some_and(|(id, _, _)| id == r.id) {
                        probe_answered(&mut s, r);
                    } else if s.probe_running == Some(r.id) {
                        s.probe_early.get_or_insert(r);
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
        let mut last_path = (net::PathKind::None, String::new());
        while self.conn.close_reason().is_none() {
            tick.tick().await;
            let (k, addr, rtt) = net::selected_path(&self.conn);
            let timeout = Duration::from_secs_f64(rtt.max(0.0) * 2.0 / 1000.0).clamp(FRAME_TIMEOUT, MAX_FRAME_TIMEOUT);
            let now = Instant::now();
            let mut s = self.st.lock().unwrap();
            let before = s.in_flight.len();
            let last_sent = (s.frame > 0).then(|| s.frame.wrapping_sub(1));
            let last_was_in_flight = last_sent.is_some_and(|f| s.in_flight.contains_key(&f));
            s.in_flight.retain(|_, (at, _)| now.duration_since(*at) < timeout);
            let timed_out = before - s.in_flight.len();
            for _ in 0..timed_out {
                s.rc.on_frame_failed();
            }
            // If the very last frame sent before the screen went still lost all
            // its shards, the viewer never saw a shard or a later frame number
            // and cannot ask for a keyframe. Recover automatically once.
            if last_was_in_flight
                && let Some(f) = last_sent
                && !s.in_flight.contains_key(&f)
                && !self.paused.load(Ordering::Relaxed)
                && s.last_key_frame != Some(f)
                && s.last_timeout_rekey.is_none_or(|t| now.duration_since(t) >= timeout)
            {
                s.last_timeout_rekey = Some(now);
                self.want_key.store(true, Ordering::Relaxed);
            }
            // Another address is another network as often as not (a laptop
            // leaving a phone hotspot for WiFi stays "direct").
            if (k, &addr) != (last_path.0, &last_path.1) {
                s.rc.new_path();
                last_path = (k, addr);
            }
            let rate = s.rc.update(now);
            let fec = s.rc.fec_pct();
            let wire = (s.wire_sent - s.wire_seen) as f64 * 8.0 / REPORT_EVERY.as_secs_f64();
            s.wire_seen = s.wire_sent;
            s.wire_bps = 0.5 * s.wire_bps + 0.5 * wire;
            if !self.paused.load(Ordering::Relaxed)
                && let Some(p) = s.rc.probe(now)
            {
                // Padding tops the frames up to the probe's rate (in wire
                // bytes), and is at least a quarter of it.
                let k = 1.0 + fec as f64 / 100.0;
                let pad = (p.bps * k - s.wire_bps).max(0.25 * p.bps * k);
                let max = self.conn.max_datagram_size().unwrap_or(1200).min(MAX_DATAGRAM);
                let normal = (rate / 8.0 / self.fps.load(Ordering::Relaxed).max(1) as f64) as usize;
                // Under one frame queued, a frame is never skipped behind the
                // padding (`WHOLE_FRAMES`). The padding sent per `PROBE_STEP`
                // is at most this, so with half a frame a probe could not go
                // past about 0.8 times the rate at 120 fps, and a rate read
                // low on a LAN (3.8 Mbps on one carrying 40, 2026-11-24)
                // never climbed back.
                let max_queue = (wire_frame_bytes(normal, fec, max, s.rc.min_parity()) * 9 / 10).max(2 * max);
                s.probe_floor = Some((p.bps, now + PROBE_TIME + REPORT_EVERY));
                let quiet = now + PROBE_TIME + PROBE_QUIET;
                s.own_backlog_until = Some(s.own_backlog_until.map_or(quiet, |t| t.max(quiet)));
                s.probe_sent = None;
                s.probe_running = Some(p.id);
                s.probe_early = None;
                tokio::spawn(self.clone().send_probe(p.id, pad, max, max_queue));
            }
            let floor = match s.probe_floor {
                Some((bps, until)) if now < until => rate.max(bps),
                _ => rate,
            };
            cc::set_floor(floor, fec);
        }
        cc::clear_floor();
    }

    /// Sends probe `id`'s padding: `pad_bps` of `len`-byte datagrams for
    /// `PROBE_TIME`, while under `max_queue` bytes wait to be sent (so frames
    /// are not skipped behind it). Stops early once the round trip has grown
    /// by `PROBE_ABORT_MS`: the link is full, and more padding only queues.
    async fn send_probe(self: Arc<Self>, id: u16, pad_bps: f64, len: usize, max_queue: usize) {
        let rtt = || net::selected_path(&self.conn).2;
        let rtt0 = rtt();
        let wire0 = self.st.lock().unwrap().wire_sent;
        let start = Instant::now();
        let (mut sent, mut held) = (0usize, false);
        // Wire bytes so far (frames and padding), and when: just after the
        // first padding datagram and after the latest. The viewer counts what
        // arrives between its first and last padding datagram, so a frame
        // handed over after the last one (it arrives after it) must not count
        // as sent: a key frame there read a LAN carrying 40 Mbps at 3.8
        // (2026-11-24).
        let wire_now = |sent: usize| self.st.lock().unwrap().wire_sent + sent as u64;
        let mut first: Option<(Instant, u64)> = None;
        let mut last: Option<(Instant, u64)> = None;
        let mut tick = tokio::time::interval(PROBE_STEP);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let datagram = Bytes::from(probe_datagram(id, len));
        loop {
            tick.tick().await;
            let elapsed = start.elapsed();
            if elapsed >= PROBE_TIME || rtt() - rtt0 > PROBE_ABORT_MS || self.conn.close_reason().is_some() {
                break;
            }
            let due = (pad_bps / 8.0 * elapsed.as_secs_f64()) as usize;
            let before = sent;
            while sent < due {
                let queued =
                    SEND_BUFFER.saturating_sub(self.conn.datagram_send_buffer_space()) + self.pacer_bytes.load(Ordering::Relaxed);
                if queued > max_queue {
                    held = true;
                    break;
                }
                if self.conn.send_datagram(datagram.clone()).is_err() {
                    return;
                }
                sent += len;
                self.st.lock().unwrap().padding_sent += len as u64;
                if first.is_none() {
                    first = Some((Instant::now(), wire_now(sent)));
                }
            }
            if sent > before {
                last = Some((Instant::now(), wire_now(sent)));
            }
        }
        let bps = match (first, last) {
            (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => (b1 - b0) as f64 * 8.0 / (t1 - t0).as_secs_f64(),
            _ => (wire_now(sent) - wire0) as f64 * 8.0 / start.elapsed().as_secs_f64().max(1e-3),
        };
        let mut s = self.st.lock().unwrap();
        if s.probe_running != Some(id) {
            // A newer probe started meanwhile.
            return;
        }
        s.probe_running = None;
        s.probe_sent = Some((id, bps, held));
        if let Some(r) = s.probe_early.take() {
            probe_answered(&mut s, r);
        }
    }
}

/// The viewer answered the probe in `s.probe_sent` with `r`.
fn probe_answered(s: &mut State, r: ProbeResult) {
    let Some((id, sent, held)) = s.probe_sent.take() else { return };
    // Both in wire bytes; the rate controller counts frame bytes.
    let wire = 1.0 + s.rc.fec_pct() as f64 / 100.0;
    let got = r.bps();
    crate::log!(
        "probe {id}: {:.1} Mbps sent, {:.1} got through{}",
        sent / 1e6,
        got / 1e6,
        if held { " (padding held back by our own queue)" } else { "" }
    );
    s.rc.on_probe(Instant::now(), id, sent / wire, got / wire, held);
}

/// Estimates the wire datagram bytes (payload + FEC parity + shard headers)
/// emitted for a frame of `payload` bytes. Monotonically non-decreasing in
/// both `payload` and `fec_pct`. Parity counts as its share of the payload,
/// under what a small frame's whole-shard parity really is; the queue limits
/// that use it are set for this. With the exact size, frames queued longer
/// and the picture took longer to come back after the link dropped from 8 to
/// 1.5 Mbps (netshape, 2026-11-25).
fn wire_frame_bytes(payload: usize, fec_pct: u32, max_datagram: usize, min_parity: u16) -> usize {
    let max_payload = (max_datagram.saturating_sub(SHARD_HDR) & !1).max(2);
    let data_shards = payload.div_ceil(max_payload).max(1);
    let parity_shards = parity_count(data_shards, fec_pct, min_parity);
    let wire_payload = payload.saturating_mul(100 + fec_pct as usize).div_ceil(100);
    wire_payload.saturating_add((data_shards + parity_shards) * SHARD_HDR)
}

/// Tracks queued `pacer_bytes` for one frame in `pace()`, subtracting each
/// packet immediately after `send_datagram` and any remaining bytes on drop.
struct PacerBytesGuard<'a> {
    counter: &'a AtomicUsize,
    remaining: usize,
}

impl PacerBytesGuard<'_> {
    fn take(&mut self, bytes: usize) {
        let sub = bytes.min(self.remaining);
        self.remaining -= sub;
        if sub > 0 {
            self.counter.fetch_sub(sub, Ordering::Relaxed);
        }
    }
}

impl Drop for PacerBytesGuard<'_> {
    fn drop(&mut self) {
        if self.remaining > 0 {
            self.counter.fetch_sub(self.remaining, Ordering::Relaxed);
        }
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
    pacer_bytes: Arc<AtomicUsize>,
    ctrl_pending: Arc<AtomicBool>,
) {
    let ctrl_backlog_threshold = MAX_DATAGRAM * 4;
    while let Some(frame) = rx.recv().await {
        let mut guard = PacerBytesGuard { counter: &pacer_bytes, remaining: frame.bytes };
        let spread = pace_spread(frame.packets.len(), frame.bytes, fps.load(Ordering::Relaxed), frame.link_bps, frame.fast_lan);
        let plan = pacer::plan(frame.packets.len(), spread, PACE_GAP);
        let start = tokio::time::Instant::now();
        let mut it = frame.packets.into_iter();
        let mut yielded_for_ctrl = false;
        for (off, n) in plan {
            if !off.is_zero() {
                tokio::time::sleep_until(start + off).await;
            }
            for p in it.by_ref().take(n) {
                if !yielded_for_ctrl && ctrl_pending.swap(false, Ordering::AcqRel) {
                    // `noq-proto` packs DATAGRAM frames ahead of STREAM frames;
                    // only yield when outgoing datagrams have actually built up,
                    // avoiding repeated coarse 1-2 ms Windows timer sleeps on
                    // every packet.
                    if SEND_BUFFER.saturating_sub(conn.datagram_send_buffer_space()) > ctrl_backlog_threshold {
                        yielded_for_ctrl = true;
                        tokio::task::yield_now().await;
                        if SEND_BUFFER.saturating_sub(conn.datagram_send_buffer_space()) > ctrl_backlog_threshold {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    }
                }
                let len = p.len();
                let _ = conn.send_datagram(p);
                guard.take(len);
            }
        }
        drop(guard);
        if conn.close_reason().is_some() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_stream::rate::MIN_PARITY;

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

    #[test]
    fn wire_frame_bytes_is_monotonic_and_accounts_for_fec_and_headers() {
        for fec_pct in [0, 10, 20, 50] {
            let mut prev = 0;
            for payload in (1..=10_000).chain((10_000..=150_000).step_by(977)) {
                let wire = wire_frame_bytes(payload, fec_pct, MAX_DATAGRAM, MIN_PARITY);
                assert!(wire >= prev, "non-monotonic at payload={payload}, fec={fec_pct}: {wire} < {prev}");
                assert!(wire > payload, "missing header/FEC overhead at payload={payload}");
                prev = wire;
            }
        }

        // Monotonic in FEC percentage as well.
        let payload = 50_000;
        let w0 = wire_frame_bytes(payload, 0, MAX_DATAGRAM, MIN_PARITY);
        let w10 = wire_frame_bytes(payload, 10, MAX_DATAGRAM, MIN_PARITY);
        let w50 = wire_frame_bytes(payload, 50, MAX_DATAGRAM, MIN_PARITY);
        assert!(w0 < w10 && w10 < w50);
        // 50% FEC adds ~50% payload plus parity shard headers.
        assert!(w50 >= payload * 3 / 2);
    }

    #[test]
    fn pacer_bytes_guard_subtracts_per_packet_and_on_early_drop() {
        let counter = AtomicUsize::new(5000);
        {
            let mut g = PacerBytesGuard { counter: &counter, remaining: 3000 };
            g.take(1200);
            assert_eq!(counter.load(Ordering::Relaxed), 3800);
            g.take(1200);
            assert_eq!(counter.load(Ordering::Relaxed), 2600);
            // Simulate early exit / task cancellation with 600 bytes still unsent.
        }
        assert_eq!(counter.load(Ordering::Relaxed), 2000);

        // Guard clamps to `remaining` so it cannot underflow `counter`.
        {
            let mut g = PacerBytesGuard { counter: &counter, remaining: 1000 };
            g.take(1500);
            assert_eq!(counter.load(Ordering::Relaxed), 1000);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 1000);
    }
}
