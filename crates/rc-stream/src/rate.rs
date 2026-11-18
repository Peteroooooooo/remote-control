//! Picks the encoder bitrate and the FEC share.
//!
//! The rate drops only when the link cannot carry it: the best 100 ms of
//! delivery (from acks) in the last second stays below the chosen rate while
//! frames pile up at the sender, or too many frames cannot be decoded while
//! delivery also falls short. Frames lost to bursts of WiFi loss on a link
//! that still delivers what was offered are not congestion: a lower rate does
//! not stop the bursts (5% loss in runs of 8, 2026-11-09: cut from 20 to
//! 2.4 Mbps, 8 s to climb back). They raise the FEC share instead.
//! Round-trip delay is not used to cut: on home WiFi (especially with power
//! saving on) frames wait 50-400 ms even on an idle link, and a lower rate does
//! not remove that. Stale frames are dropped by the sender instead, so our own
//! queue stays under one frame. Random loss raises the FEC share.
//!
//! Delivery is compared with what the sender offered (`on_frame_offered`), not
//! with the chosen rate: a still desktop makes the encoder produce tiny frames,
//! and that must not look like a slow link.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::wire::Report;

#[derive(Clone, Debug)]
pub struct RateConfig {
    pub min_bps: f64,
    pub max_bps: f64,
    pub start_bps: f64,
    pub min_fec_pct: u32,
    pub max_fec_pct: u32,
}

impl RateConfig {
    pub fn new(min_bps: f64, max_bps: f64) -> Self {
        RateConfig { min_bps, max_bps, start_bps: max_bps, min_fec_pct: 10, max_fec_pct: 50 }
    }
}

const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Delivery slots kept: one per update, about one second.
const SLOTS: usize = 10;
const WARMUP: Duration = Duration::from_millis(500);
/// Best slot / chosen rate below which the link counts as too slow.
const KEEPING_UP: f64 = 0.95;
/// Below this the link is clearly overloaded.
const OVERLOADED: f64 = 0.5;
/// Offered load / chosen rate below which the link is not judged at all.
const APP_LIMITED: f64 = 0.25;
const HOLD_AFTER_CUT: Duration = Duration::from_millis(500);
const MIN_CUT_GAP: Duration = Duration::from_millis(200);
/// WiFi loses datagrams in runs. A run longer than a frame's parity loses the
/// frame although the average loss looks covered, and the average falls back
/// between runs. So every frame that fails despite FEC adds this much parity
/// (percentage points, up to `BURST_MAX`), which then fades by `BURST_DECAY`
/// points per second. At 5% loss in runs of 8 datagrams (viewer `--sim-loss 5
/// --sim-burst 8`) this halved the failed frames.
const BURST_STEP: f64 = 10.0;
const BURST_MAX: f64 = 30.0;
const BURST_DECAY: f64 = 2.0;

pub struct RateController {
    cfg: RateConfig,
    rate: f64,
    started: Instant,
    last_update: Instant,
    rtts: Vec<f64>,
    minima: VecDeque<(Instant, f64)>,
    acked_bytes: u64,
    offered_bytes: u64,
    /// (delivered bits/s, chosen bits/s, offered bits/s) per update.
    slots: VecDeque<(f64, f64, f64)>,
    failed: u32,
    backlog: bool,
    loss: f64,
    hold_until: Instant,
    next_cut: Instant,
    queue_ms: f64,
    delivery: f64,
    /// Extra parity in percent while whole frames still fail despite FEC.
    burst_pct: f64,
}

impl RateController {
    pub fn new(cfg: RateConfig, now: Instant) -> Self {
        let rate = cfg.start_bps.clamp(cfg.min_bps, cfg.max_bps);
        RateController {
            cfg,
            rate,
            started: now,
            last_update: now,
            rtts: Vec::new(),
            minima: VecDeque::new(),
            acked_bytes: 0,
            offered_bytes: 0,
            slots: VecDeque::new(),
            failed: 0,
            backlog: false,
            loss: 0.0,
            hold_until: now,
            next_cut: now,
            queue_ms: 0.0,
            delivery: 0.0,
            burst_pct: 0.0,
        }
    }

    /// A frame of `bytes` was acked `ms` after its first datagram went out.
    pub fn on_frame_acked(&mut self, ms: f64, bytes: u64) {
        self.rtts.push(ms);
        self.acked_bytes += bytes;
    }

    /// A frame of `bytes` was due: its encoded size if sent as is, or the size the
    /// bitrate allows if it was shrunk or skipped because of a backlog.
    pub fn on_frame_offered(&mut self, bytes: u64) {
        self.offered_bytes += bytes;
    }

    /// A frame was never acked (lost, or not decodable).
    pub fn on_frame_failed(&mut self) {
        self.failed += 1;
    }

    /// The sender found unsent data still queued when the next frame was due.
    pub fn on_backlog(&mut self) {
        self.backlog = true;
    }

    pub fn on_report(&mut self, r: &Report) {
        if r.shards_expected > 0 {
            self.loss = 0.7 * self.loss + 0.3 * r.shard_loss();
        }
        if r.frames_failed > 0 {
            self.burst_pct = (self.burst_pct + BURST_STEP * r.frames_failed as f64).min(BURST_MAX);
        }
    }

    /// Forget the minimum round trip, e.g. after switching between relay and direct path.
    pub fn reset_base(&mut self) {
        self.minima.clear();
    }

    /// Call about every 100 ms. Returns the new target bitrate in bits per second.
    pub fn update(&mut self, now: Instant) -> f64 {
        let mut s = std::mem::take(&mut self.rtts);
        s.sort_by(f64::total_cmp);
        if let Some(&min) = s.first() {
            self.minima.push_back((now, min));
        }
        while self.minima.front().is_some_and(|(t, _)| now.duration_since(*t) > BASE_WINDOW) {
            self.minima.pop_front();
        }
        let base = self.minima.iter().map(|m| m.1).fold(f64::INFINITY, f64::min);
        // The median, not a high percentile: random WiFi stalls hit a few frames;
        // a queue that our own traffic builds raises the median.
        self.queue_ms = match s.last() {
            Some(_) => s[(s.len() - 1) / 2] - base,
            None => 0.0,
        };

        // What the link can carry: the best 100 ms of delivery in the last second.
        // A WiFi stall delivers nothing for a while and then everything at once,
        // so the best slot still shows the real capacity; a link that is too slow
        // for the chosen rate never reaches it in any slot.
        let dt = now.duration_since(self.last_update).as_secs_f64().max(1e-3);
        self.last_update = now;
        self.burst_pct = (self.burst_pct - BURST_DECAY * dt).max(0.0);
        let bits = std::mem::take(&mut self.acked_bytes) as f64 * 8.0;
        let offered = std::mem::take(&mut self.offered_bytes) as f64 * 8.0;
        self.slots.push_back((bits / dt, self.rate, offered / dt));
        while self.slots.len() > SLOTS {
            self.slots.pop_front();
        }
        self.delivery = self.slots.iter().map(|s| s.0).fold(0.0, f64::max);
        let n = self.slots.len() as f64;
        let chosen = self.slots.iter().map(|s| s.1).sum::<f64>() / n;
        let demand = self.slots.iter().map(|s| s.2).sum::<f64>() / n;
        let judged = now.duration_since(self.started) >= WARMUP && demand >= APP_LIMITED * chosen;
        let limited = judged && self.delivery < KEEPING_UP * demand;
        let overloaded = judged && self.delivery < OVERLOADED * demand;

        let failing = self.failed > 2 && limited;
        let cut = overloaded || failing || (limited && self.backlog);
        if cut {
            if now >= self.next_cut {
                let heavy = overloaded || failing;
                self.rate *= if heavy { 0.7 } else { 0.85 };
                self.next_cut = now + MIN_CUT_GAP;
            }
            self.hold_until = now + HOLD_AFTER_CUT;
        } else if now >= self.hold_until && !limited {
            self.rate *= 1.04;
        }
        self.rate = self.rate.clamp(self.cfg.min_bps, self.cfg.max_bps);
        self.failed = 0;
        self.backlog = false;
        self.rate
    }

    pub fn bitrate(&self) -> f64 {
        self.rate
    }

    /// Best delivered bits per second in a 100 ms slot over the last second.
    pub fn delivery(&self) -> f64 {
        self.delivery
    }

    /// Queue delay seen in the last update, in ms.
    pub fn queue_ms(&self) -> f64 {
        self.queue_ms
    }

    /// Smoothed shard loss, 0.0..=1.0.
    pub fn loss(&self) -> f64 {
        self.loss
    }

    /// Parity shards as a percentage of data shards: the minimum plus three times
    /// the loss rate (so 5% loss gives 25%), plus the burst allowance.
    pub fn fec_pct(&self) -> u32 {
        let pct = self.cfg.min_fec_pct as f64 + 3.0 * self.loss * 100.0 + self.burst_pct;
        (pct.ceil() as u32).clamp(self.cfg.min_fec_pct, self.cfg.max_fec_pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: f64 = 1e6;

    /// One 100 ms tick on a link that carries at most `cap` bits per second.
    /// Above capacity the sender sees a backlog and frames wait `queue_ms` longer.
    fn tick(c: &mut RateController, t: &mut Instant, rtt: f64, cap: f64, queue_ms: f64) {
        let sent = c.bitrate().min(cap);
        let over = c.bitrate() > cap;
        *t += Duration::from_millis(100);
        for _ in 0..6 {
            let bytes = (sent / 8.0 / 60.0) as u64;
            c.on_frame_offered((c.bitrate() / 8.0 / 60.0) as u64);
            c.on_frame_acked(if over { rtt + queue_ms } else { rtt }, bytes);
        }
        if over {
            c.on_backlog();
        }
        c.update(*t);
    }

    fn run(c: &mut RateController, t: &mut Instant, ticks: u32, rtt: f64, cap: f64) {
        for _ in 0..ticks {
            tick(c, t, rtt, cap, 80.0);
        }
    }

    #[test]
    fn grows_to_max_when_link_keeps_up() {
        let mut t = Instant::now();
        let mut cfg = RateConfig::new(2.0 * MBPS, 20.0 * MBPS);
        cfg.start_bps = 2.0 * MBPS;
        let mut c = RateController::new(cfg, t);
        run(&mut c, &mut t, 100, 5.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 20.0 * MBPS);
    }

    #[test]
    fn settles_near_capacity_then_recovers() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 10, 5.0, 100.0 * MBPS);
        run(&mut c, &mut t, 100, 5.0, 8.0 * MBPS);
        let low = c.bitrate();
        assert!((5.0 * MBPS..=9.0 * MBPS).contains(&low), "rate on an 8 Mbps link: {low}");
        run(&mut c, &mut t, 100, 5.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 20.0 * MBPS);
    }

    #[test]
    fn high_delay_alone_does_not_cut() {
        // WiFi power saving: every frame waits ~150 ms, but everything arrives.
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 10, 5.0, 100.0 * MBPS);
        run(&mut c, &mut t, 50, 150.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 20.0 * MBPS);
    }

    #[test]
    fn failed_frames_cut_but_shard_loss_only_raises_fec() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        for _ in 0..20 {
            c.on_report(&Report { shards_expected: 100, shards_received: 95, ..Default::default() });
        }
        run(&mut c, &mut t, 10, 5.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 20.0 * MBPS);
        assert_eq!(c.fec_pct(), 25);
        c.on_frame_failed();
        tick(&mut c, &mut t, 5.0, 100.0 * MBPS, 0.0);
        assert_eq!(c.bitrate(), 20.0 * MBPS, "one lost frame is not congestion");
        for _ in 0..3 {
            c.on_frame_failed();
        }
        tick(&mut c, &mut t, 5.0, 100.0 * MBPS, 0.0);
        assert_eq!(c.bitrate(), 20.0 * MBPS, "frames lost on a link that delivers everything else are not congestion");
        // The same failures while delivery falls short of what was offered: a heavy cut.
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 10, 5.0, 100.0 * MBPS);
        for _ in 0..10 {
            t += Duration::from_millis(100);
            c.on_frame_offered((c.bitrate() / 8.0 / 10.0) as u64);
            c.on_frame_acked(5.0, (c.bitrate() / 8.0 / 10.0 * 0.9) as u64);
            c.update(t);
        }
        assert_eq!(c.bitrate(), 20.0 * MBPS, "10% short alone, without a backlog, is no cut");
        for _ in 0..3 {
            c.on_frame_failed();
        }
        t += Duration::from_millis(100);
        c.on_frame_offered((c.bitrate() / 8.0 / 10.0) as u64);
        c.on_frame_acked(5.0, (c.bitrate() / 8.0 / 10.0 * 0.9) as u64);
        c.update(t);
        assert!((c.bitrate() - 14.0 * MBPS).abs() < 1.0, "{}", c.bitrate());
    }

    #[test]
    fn frames_failing_despite_fec_add_parity_that_fades() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 10, 5.0, 100.0 * MBPS);
        assert_eq!(c.fec_pct(), 10);
        c.on_report(&Report { shards_expected: 400, shards_received: 380, frames_failed: 1, ..Default::default() });
        // 10 minimum + 3 x 1.5% smoothed loss + 10 for the failed frame.
        assert_eq!(c.fec_pct(), 25);
        for _ in 0..5 {
            c.on_report(&Report { frames_failed: 1, ..Default::default() });
        }
        assert!(c.fec_pct() <= 50);
        // No more failures and no shard loss: back to the minimum within about 20 s.
        for _ in 0..200 {
            c.on_report(&Report { shards_expected: 100, shards_received: 100, ..Default::default() });
            tick(&mut c, &mut t, 5.0, 100.0 * MBPS, 0.0);
        }
        assert_eq!(c.fec_pct(), 10);
        assert_eq!(c.bitrate(), 20.0 * MBPS, "failed frames in reports are not congestion");
    }

    #[test]
    fn ignores_random_spikes() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        for i in 0..100 {
            // Five of six frames are quick; one in six hits a 150 ms WiFi stall.
            t += Duration::from_millis(100);
            for j in 0..6 {
                let bytes = (c.bitrate() / 8.0 / 60.0) as u64;
                c.on_frame_offered(bytes);
                c.on_frame_acked(if (i + j) % 6 == 0 { 150.0 } else { 5.0 }, bytes);
            }
            c.update(t);
        }
        assert_eq!(c.bitrate(), 20.0 * MBPS);
    }

    #[test]
    fn never_below_min() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        for _ in 0..100 {
            c.on_frame_offered((c.bitrate() / 8.0 / 60.0) as u64 * 18);
            c.on_backlog();
            t += Duration::from_millis(300);
            c.update(t);
        }
        assert_eq!(c.bitrate(), 2.0 * MBPS);
    }

    #[test]
    fn still_desktop_does_not_cut() {
        // Tiny frames (a blinking cursor), all delivered: the link is not judged.
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        for i in 0..100 {
            t += Duration::from_millis(100);
            if i % 5 == 0 {
                c.on_frame_offered(2_000);
                c.on_frame_acked(5.0, 2_000);
            }
            c.update(t);
        }
        assert_eq!(c.bitrate(), 20.0 * MBPS);
    }

    #[test]
    fn big_frame_after_still_period_is_not_overload() {
        // One 300 KB frame (a window opening) delivered over 150 ms, then quiet.
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 10, 5.0, 100.0 * MBPS);
        c.on_frame_offered(300_000);
        for k in 0..20 {
            t += Duration::from_millis(100);
            if k == 1 {
                c.on_frame_acked(150.0, 300_000);
            }
            c.update(t);
        }
        assert_eq!(c.bitrate(), 20.0 * MBPS);
    }
}
