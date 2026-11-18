//! Viewer side of the video stream: rebuilds frames from shards, acks them at
//! once (the host's rate control needs that), and hands them to the decoder in
//! order. H.264 delta frames depend on the frame before, so a lost frame means
//! waiting for a key frame, which is requested right away.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use iroh::endpoint::Connection;
use rc_stream::{
    fec::Assembler,
    wire::{Ack, ShardHeader, T_SHARD},
};
use tokio::sync::mpsc;

use crate::{
    proto::{FrameHeader, Msg},
    video_tx::REPORT_EVERY,
};

/// How long the FEC assembler keeps a frame open after its latest shard.
const FRAME_LINGER: Duration = Duration::from_millis(150);
/// A later frame is complete but this one is not: give it this long.
const GAP_WAIT: Duration = Duration::from_millis(30);
/// No shard of an incomplete frame for this long, and nothing came after it.
/// Counted from the latest shard, so a large frame on a slow link may take
/// as long as it needs.
const STUCK_WAIT: Duration = Duration::from_millis(120);
const KEY_REQUEST_GAP: Duration = Duration::from_millis(300);
/// How often missing frames are looked for. Every millisecond here is a
/// millisecond of frozen picture after a loss (the report interval, 100 ms,
/// made the freeze about 50 ms longer on average).
const CHECK_EVERY: Duration = Duration::from_millis(10);

pub struct Delivered {
    pub header: FrameHeader,
    /// H.264 Annex B bytes.
    pub data: Vec<u8>,
    pub complete_at: Instant,
    /// Frame number on the wire (for `trace`).
    pub frame: u32,
}

#[derive(Default)]
pub struct Reorder {
    next: Option<u32>,
    pending: BTreeMap<u32, Delivered>,
    gap_since: Option<Instant>,
    incomplete: BTreeMap<u32, Instant>,
    need_key: bool,
    first_seen: Option<Instant>,
    last_request: Option<Instant>,
    /// When the latest loss was noticed, and when the last frame before it
    /// went to the decoder; kept until the key frame arrives.
    lost_at: Option<(Instant, Option<Instant>)>,
    last_out: Option<Instant>,
    /// Set when a key frame ends a loss: how long after the loss was noticed,
    /// and how long the picture stood still because of it.
    pub recovered: Option<(Duration, Option<Duration>)>,
    pub key_requests: u64,
    pub lost: u64,
}

impl Reorder {
    /// A shard of `frame` arrived.
    pub fn on_shard(&mut self, frame: u32, now: Instant) {
        self.first_seen.get_or_insert(now);
        if self.next.is_some_and(|n| frame < n) || self.pending.contains_key(&frame) {
            return;
        }
        self.incomplete.insert(frame, now);
    }

    /// A frame could be rebuilt. Returns the frames that can be decoded now, in order.
    pub fn push(&mut self, frame: u32, d: Delivered, now: Instant) -> Vec<Delivered> {
        self.incomplete.remove(&frame);
        let mut out = Vec::new();
        let behind = self.next.is_some_and(|n| frame < n);
        if d.header.key && !behind {
            self.pending.retain(|f, _| *f > frame);
            if self.need_key {
                self.recovered = self
                    .lost_at
                    .take()
                    .map(|(t, before)| (now.duration_since(t), before.map(|b| now.duration_since(b))));
            }
            self.need_key = false;
            // The request gap spaces out repeats while a key frame is on its
            // way; a loss after it arrived is asked for at once.
            self.last_request = None;
            out.push(d);
            self.next = Some(frame.wrapping_add(1));
        } else if self.need_key || behind {
            return out;
        } else {
            match self.next {
                Some(n) if frame == n => {
                    out.push(d);
                    self.next = Some(n.wrapping_add(1));
                }
                Some(_) => {
                    self.pending.insert(frame, d);
                }
                None => return out,
            }
        }
        while let Some(n) = self.next {
            let Some(d) = self.pending.remove(&n) else { break };
            out.push(d);
            self.next = Some(n.wrapping_add(1));
        }
        // The wait for the missing frame restarts whenever something was delivered.
        self.gap_since = match (self.pending.is_empty(), out.is_empty()) {
            (true, _) => None,
            (false, false) => Some(now),
            (false, true) => Some(self.gap_since.unwrap_or(now)),
        };
        if !out.is_empty() {
            self.last_out = Some(now);
        }
        out
    }

    /// Call every `CHECK_EVERY`. True when a key frame should be requested now.
    pub fn tick(&mut self, now: Instant) -> bool {
        if let Some(n) = self.next {
            self.incomplete.retain(|f, _| *f >= n);
        }
        let gap = self.gap_since.is_some_and(|t| now.duration_since(t) > GAP_WAIT);
        let stuck = self.incomplete.values().any(|t| now.duration_since(*t) > STUCK_WAIT);
        if (gap || stuck) && !self.need_key {
            self.need_key = true;
            self.lost += 1;
            self.lost_at = Some((now, self.last_out));
        }
        if gap || stuck {
            self.pending.clear();
            self.incomplete.clear();
            self.gap_since = None;
        }
        let no_start = self.next.is_none() && self.first_seen.is_some_and(|t| now.duration_since(t) > STUCK_WAIT);
        let want = self.need_key || no_start;
        if want && self.last_request.is_none_or(|t| now.duration_since(t) >= KEY_REQUEST_GAP) {
            self.last_request = Some(now);
            self.key_requests += 1;
            return true;
        }
        false
    }
}

#[derive(Default)]
pub struct RxCounters {
    pub frames: u64,
    pub bytes: u64,
    pub recovered: u64,
    pub key_requests: u64,
    pub lost: u64,
    /// Frames that never had enough shards (from the FEC assembler).
    pub failed: u64,
    /// Datagrams dropped by `--sim-loss`.
    pub sim_dropped: u64,
    /// Parity shards per 100 data shards of the largest frame.
    pub fec_pct: u32,
    largest: u16,
}

/// Runs until the connection closes. Decodable frames go to `frames`; key
/// frame requests go to the control stream writer through `ctrl`.
/// `sim` drops some received video datagrams (for tests).
pub async fn run(
    conn: Connection,
    frames: std::sync::mpsc::Sender<Delivered>,
    ctrl: mpsc::UnboundedSender<Msg>,
    counters: Arc<Mutex<RxCounters>>,
    mut sim: Option<crate::lossim::LossSim>,
) {
    let mut asm = Assembler::new(FRAME_LINGER);
    let mut ro = Reorder::default();
    let mut tick = tokio::time::interval(REPORT_EVERY);
    let mut check = tokio::time::interval(CHECK_EVERY);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_first: Option<u32> = None;
    loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let Ok(d) = d else { return };
                if d.first() != Some(&T_SHARD) {
                    continue;
                }
                if sim.as_mut().is_some_and(|s| s.drop_next()) {
                    counters.lock().unwrap().sim_dropped += 1;
                    continue;
                }
                let now = Instant::now();
                if let Some((h, _)) = ShardHeader::parse(&d) {
                    ro.on_shard(h.frame, now);
                    if h.index == 0 || h.index == h.data {
                        let mut k = counters.lock().unwrap();
                        if h.data >= k.largest {
                            k.largest = h.data;
                            k.fec_pct = h.parity as u32 * 100 / h.data.max(1) as u32;
                        }
                    }
                    if crate::trace::on() && last_first.is_none_or(|f| h.frame > f) {
                        last_first = Some(h.frame);
                        crate::trace::mark(h.frame, "first_shard", None);
                    }
                }
                let Some(c) = asm.push(&d, now) else { continue };
                crate::trace::mark(c.frame, "complete", None);
                let _ = conn.send_datagram(Ack { frame: c.frame, recovered: c.recovered }.encode().into());
                let Some((header, body)) = FrameHeader::parse(&c.data) else { continue };
                {
                    let mut k = counters.lock().unwrap();
                    k.frames += 1;
                    k.bytes += c.data.len() as u64;
                    k.recovered += c.recovered as u64;
                }
                let d = Delivered { header, data: body.to_vec(), complete_at: now, frame: c.frame };
                for f in ro.push(c.frame, d, now) {
                    if frames.send(f).is_err() {
                        return;
                    }
                }
                if let Some((after, still)) = ro.recovered.take() {
                    crate::log!(
                        "video: key frame {} ms after the loss was noticed, picture still for {} ms",
                        after.as_millis(),
                        still.map_or(0, |s| s.as_millis())
                    );
                }
            }
            _ = check.tick() => {
                let now = Instant::now();
                let lost = ro.lost;
                let asked = ro.tick(now);
                if asked {
                    let _ = ctrl.send(Msg::RequestKeyframe);
                }
                match (ro.lost > lost, asked) {
                    (true, true) => crate::log!("video: frame {} lost, asking for a key frame", ro.next.unwrap_or(0)),
                    (true, false) => crate::log!("video: frame {} lost, key frame asked for shortly before", ro.next.unwrap_or(0)),
                    (false, true) => crate::log!("video: asking for a key frame again"),
                    (false, false) => {}
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                asm.expire(now);
                let r = asm.take_report();
                if r.shards_expected > 0 {
                    let _ = conn.send_datagram(r.encode().into());
                }
                let mut k = counters.lock().unwrap();
                k.key_requests = ro.key_requests;
                k.lost = ro.lost;
                k.failed += r.frames_failed as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(key: bool) -> Delivered {
        Delivered {
            header: FrameHeader { key, hevc: false, width: 64, height: 64, host_us: 0 },
            data: vec![],
            complete_at: Instant::now(),
            frame: 0,
        }
    }

    fn ids(v: &[Delivered], first: u32) -> Vec<u32> {
        (first..first + v.len() as u32).collect()
    }

    #[test]
    fn waits_for_first_key_frame() {
        let mut r = Reorder::default();
        let t = Instant::now();
        r.on_shard(0, t);
        assert!(r.push(0, frame(false), t).is_empty());
        assert!(r.tick(t + Duration::from_millis(200)), "asks for a key frame");
        assert_eq!(r.push(1, frame(true), t).len(), 1);
        assert_eq!(r.push(2, frame(false), t).len(), 1);
    }

    #[test]
    fn reorders_and_flushes() {
        let mut r = Reorder::default();
        let t = Instant::now();
        assert_eq!(r.push(0, frame(true), t).len(), 1);
        assert!(r.push(2, frame(false), t).is_empty());
        let out = r.push(1, frame(false), t);
        assert_eq!(ids(&out, 1), vec![1, 2]);
        assert!(!r.tick(t + Duration::from_millis(100)));
    }

    #[test]
    fn gap_asks_for_key_and_drops_deltas() {
        let mut r = Reorder::default();
        let t = Instant::now();
        r.push(0, frame(true), t);
        r.on_shard(1, t);
        assert!(r.push(2, frame(false), t).is_empty());
        assert!(r.tick(t + Duration::from_millis(50)));
        assert_eq!(r.lost, 1);
        assert!(r.push(3, frame(false), t).is_empty(), "delta frames are useless until a key frame");
        assert!(!r.tick(t + Duration::from_millis(100)), "requests are spaced out");
        assert!(r.tick(t + Duration::from_millis(400)), "asks again");
        assert_eq!(r.push(5, frame(true), t).len(), 1);
        assert_eq!(r.push(6, frame(false), t).len(), 1);
        assert_eq!(r.lost, 1);
    }

    #[test]
    fn a_new_loss_after_recovery_is_asked_for_at_once() {
        let mut r = Reorder::default();
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        r.push(0, frame(true), t);
        r.on_shard(1, t);
        r.push(2, frame(false), t);
        assert!(r.tick(ms(40)));
        assert_eq!(r.push(3, frame(true), ms(100)).len(), 1);
        assert!(r.recovered.is_some());
        r.on_shard(4, ms(100));
        r.push(5, frame(false), ms(110));
        assert!(r.tick(ms(150)), "not held back by the request 110 ms ago");
        assert_eq!(r.lost, 2);
    }

    #[test]
    fn stuck_last_frame_is_noticed() {
        let mut r = Reorder::default();
        let t = Instant::now();
        r.push(0, frame(true), t);
        r.on_shard(1, t);
        assert!(!r.tick(t + Duration::from_millis(100)));
        assert!(r.tick(t + Duration::from_millis(200)));
    }

    #[test]
    fn slow_frame_still_arriving_is_not_lost() {
        let mut r = Reorder::default();
        let t = Instant::now();
        r.push(0, frame(true), t);
        for i in 0..10 {
            let now = t + Duration::from_millis(50 * i);
            r.on_shard(1, now);
            assert!(!r.tick(now + Duration::from_millis(40)), "shards keep coming at {i}");
        }
        assert_eq!(r.push(1, frame(false), t + Duration::from_millis(500)).len(), 1);
        assert_eq!(r.lost, 0);
    }

    #[test]
    fn frames_before_a_key_frame_do_not_count_as_lost() {
        let mut r = Reorder::default();
        let t = Instant::now();
        r.push(0, frame(true), t);
        r.on_shard(1, t);
        r.push(2, frame(true), t);
        assert!(!r.tick(t + Duration::from_millis(500)));
        assert_eq!(r.lost, 0);
    }
}
