//! Picks the encoder bitrate and the FEC share.
//!
//! The rate droprobe.as_ref().map(|p| (p.p.bps / 1e6, p.sending, p.steps))s only when the link cannot carry it: the best 100 ms of
//! delivery (from acks) in the last second stays below the chosen rate while
//! frames pile up at the sender, or too many frames cannot be decoded while
//! delivery also falls short. Frames lost to bursts of WiFi loss on a link
//! that still delivers what was offered are not congestion: a lower rate does
//! not stop the bursts (5% loss in runs of 8, 2026-11-09: cut from 20 to
//! 2.4 Mbps, 8 s to climb back). They raise the FEC share instead.
//! Round-trip delay alone does not cut: on home WiFi (especially with power
//! saving on) frames wait 50-400 ms even on an idle link, and a lower rate does
//! not remove that; only delay while less gets through than was offered
//! does (a queue in the link, `STANDING_QUEUE_MS`). Stale frames are dropped
//! by the sender instead, so our own queue stays under one frame. Random
//! loss raises the FEC share.
//!
//! Delivery is compared with what the sender offered (`on_frame_offered`), not
//! with the chosen rate: a still desktop makes the encoder produce tiny frames,
//! and that must not look like a slow link.
//!
//! What the link carries (`link`) is measured, and the rate stays under
//! `HEADROOM` times it, so that motion after a still screen fits. Frames
//! cannot measure it: they depend on the picture (a still desktop fills a
//! twentieth of the rate, a dragged window all of it), and frames that never
//! fill the link show nothing about it. Without a cap the rate climbed until
//! light frames filled the link and the first heavy motion offered twice
//! what it carries (phone hotspot 2026-11-24: frames skipped for 10 s at
//! 26-38 Mbps on a link carrying about 15); capped by what frames had shown,
//! one low reading held the rate at 1.5-4.9 Mbps on a link carrying 13.
//!
//! So the sender probes (`probe`): every `PROBE_EVERY` it tops its frames up
//! with padding to `PROBE_GAIN` times the link's rate for a moment, and the
//! viewer reports what got through while the probe arrived (`on_probe`).
//! Everything through: the link carries at least that, and the next probe
//! follows soon, higher. Less: that is what it carries. Before the link is
//! known the probes start at `FIRST_PROBE` and double. A sender whose frames
//! keep backing up measures the link as well, and so does a second that
//! delivered more than `link` without any backlog.

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
/// Updates in a row without any delivery that make a stall.
const STALL: usize = 2;
/// Updates in the last second with a backlog that make the link saturated.
const SATURATED: usize = 3;
/// Share of a saturated link's delivery a cut aims for, and the deepest single cut.
const FIT: f64 = 0.9;
const MAX_CUT: f64 = 0.5;
/// Growth per update.
const GROWTH: f64 = 1.04;
/// Highest rate over the link's measured rate: frames vary in size (key
/// frames, an encoder that overshoots its rate), and the rest of the link
/// keeps them from queueing.
const HEADROOM: f64 = 0.85;
/// A probe's total rate over the link's...
const PROBE_GAIN: f64 = 1.25;
/// ...or, before the link is known, from this and twice what the last probe
/// got through. Starting at the highest rate instead would put 150 ms of up
/// to 60 Mbps into a phone hotspot carrying 5: a second of queue.
const FIRST_PROBE: f64 = 4e6;
const STARTUP_GAIN: f64 = 2.0;
/// A probe that delivered this share of what was sent got everything through.
const PROBE_FULL: f64 = 0.9;
/// Time between probes; after a probe that got everything through, or read
/// well under the link before (a burst on a phone hotspot can), the next one
/// comes after `PROBE_AGAIN`. A probe that fills the link queues at most
/// about `PROBE_ABORT_MS` there.
const PROBE_EVERY: Duration = Duration::from_secs(5);
const PROBE_AGAIN: Duration = Duration::from_millis(500);
/// A probe not answered within this time (or three round trips) is given up;
/// after `MAX_UNANSWERED` in a row the viewer is taken not to answer probes
/// at all (an older version).
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_UNANSWERED: u32 = 3;
/// The sender stops a probe early once the round trip has grown this much
/// since it started: the link is full, and more padding only queues. On a
/// link that had dropped to a third unseen, a whole probe queued 400 ms.
pub const PROBE_ABORT_MS: f64 = 40.0;
/// Frames waiting this long over the base round trip while the link falls
/// short of what was offered make a queue the sender cannot see.
const STANDING_QUEUE_MS: f64 = 100.0;
/// WiFi loses datagrams in runs. A run longer than a frame's parity loses the
/// frame although the average loss looks covered, and the average falls back
/// between runs. So every frame that fails despite FEC adds this much parity
/// (percentage points, up to `BURST_MAX`), which then fades by `BURST_DECAY`
/// points per second. At 5% loss in runs of 8 datagrams (viewer `--sim-loss 5
/// --sim-burst 8`) this halved the failed frames.
const BURST_STEP: f64 = 10.0;
const BURST_MAX: f64 = 30.0;
const BURST_DECAY: f64 = 2.0;
/// Parity shards every frame gets at least...
pub const MIN_PARITY: u16 = 2;
/// ...or as many as the longest run of shards lost from one frame lately
/// (up to `RUN_MAX`), fading by `RUN_DECAY` shards per second: a small
/// frame has a few parity shards whatever the percentage, and a run of four
/// (netshape `--loss 1 --loss-burst 4`) failed one frame in four at 5 Mbps.
/// Runs lost while frames queue are a full buffer dropping, not loss:
/// parity there only adds to the load (netshape at 1.5 Mbps: four times the
/// skipped frames).
const RUN_MAX: f64 = 8.0;
const RUN_DECAY: f64 = 0.1;
const RUN_QUEUE_MS: f64 = 50.0;

pub struct RateController {
    cfg: RateConfig,
    rate: f64,
    /// When the first frame was acked on this path.
    first_ack: Option<Instant>,
    last_update: Instant,
    rtts: Vec<f64>,
    minima: VecDeque<(Instant, f64)>,
    acked_bytes: u64,
    offered_bytes: u64,
    /// Delivery slots, one per update.
    slots: VecDeque<Slot>,
    failed: u32,
    backlog: bool,
    loss: f64,
    hold_until: Instant,
    next_cut: Instant,
    queue_ms: f64,
    delivery: f64,
    /// What the link carries, bits/s, from the last probe or backed-up frames.
    link: Option<f64>,
    /// Before `link` is known: the most a probe got through.
    shown: f64,
    /// The probe sent and not answered yet: its number, rate and start.
    probing: Option<(u16, f64, Instant)>,
    probe_id: u16,
    /// No probe before then (`None`: as soon as the link can be judged).
    next_probe: Option<Instant>,
    unanswered: u32,
    /// The link before frames last read it lower, and when: tried again by
    /// the next probe (see `lower_link`).
    before_drop: Option<(f64, Instant)>,
    /// The last update could judge the link, and saw no backlog or stall.
    warm: bool,
    calm: bool,
    /// Extra parity in percent while whole frames still fail despite FEC.
    burst_pct: f64,
    /// Longest recent run of shards lost from one frame, fading.
    run: f64,
}

/// Probe now: top the frames up with padding to `bps` in all, for a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Probe {
    pub id: u16,
    pub bps: f64,
}

/// One update's worth of traffic, in bits per second.
struct Slot {
    delivered: f64,
    chosen: f64,
    offered: f64,
    backlog: bool,
}

impl RateController {
    pub fn new(cfg: RateConfig, now: Instant) -> Self {
        let rate = cfg.start_bps.clamp(cfg.min_bps, cfg.max_bps);
        RateController {
            cfg,
            rate,
            first_ack: None,
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
            link: None,
            shown: 0.0,
            probing: None,
            probe_id: 0,
            next_probe: None,
            unanswered: 0,
            before_drop: None,
            warm: false,
            calm: false,
            burst_pct: 0.0,
            run: 0.0,
        }
    }

    /// A frame of `bytes` was acked `ms` after its first datagram went out.
    pub fn on_frame_acked(&mut self, ms: f64, bytes: u64) {
        self.rtts.push(ms);
        self.acked_bytes += bytes;
    }

    /// A frame of `bytes` was confirmed late (for example via an ACK bitmap);
    /// counts toward delivery without adding stale latency to `rtts`.
    pub fn on_frame_acked_late(&mut self, bytes: u32) {
        self.acked_bytes += bytes as u64;
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
        let queueing = self.queue_ms >= RUN_QUEUE_MS || self.backlog || self.last_update < self.hold_until;
        if !queueing {
            self.run = self.run.max((r.max_run as f64).min(RUN_MAX));
        }
    }

    /// The data now takes another path (between relay and direct, or another
    /// address): the old path's minimum round trip, its rate and the second
    /// of delivery before say nothing about the new one, which is judged
    /// afresh once it has delivered for a while (see `update`).
    pub fn new_path(&mut self) {
        self.minima.clear();
        self.link = None;
        self.shown = 0.0;
        self.probing = None;
        self.next_probe = None;
        self.before_drop = None;
        self.slots.clear();
        self.first_ack = None;
    }

    /// A new highest rate (the viewer chose another picture). A higher one is
    /// tried at once, as at the start of a session (a slow link cuts it within
    /// a second), but only up to `HEADROOM` times what the link carries; a
    /// lower one caps the rate.
    pub fn set_max(&mut self, max_bps: f64) {
        let max = max_bps.max(self.cfg.min_bps);
        self.rate = if max > self.cfg.max_bps {
            self.link.map_or(max, |l| max.min(HEADROOM * l).max(self.rate))
        } else {
            self.rate.min(max)
        };
        self.cfg.max_bps = max;
    }

    /// Frames read the link at `to`. A phone hotspot carries next to nothing
    /// for a few hundred ms every 10 s or so and then catches up: a lower
    /// reading holds until the link is calm again, and then the next probe
    /// tries what the link carried before (it stops early if that queues).
    fn lower_link(&mut self, now: Instant, to: f64) {
        if let Some(l) = self.link
            && to < l
        {
            let peak = self.before_drop.filter(|&(_, at)| now.duration_since(at) < BASE_WINDOW).map_or(l, |(p, _)| p.max(l));
            self.before_drop = Some((peak, now));
            self.next_probe = Some(now);
        }
        self.link = Some(to);
    }

    /// The highest rate the link allows now.
    fn top(&self) -> f64 {
        self.link.map_or(self.cfg.max_bps, |l| self.cfg.max_bps.min(HEADROOM * l))
    }

    /// Call after `update`. A probe to send now, if one is due: the link is
    /// not known yet, or the rate is held at `HEADROOM` times it under the
    /// highest. Frames must not be queueing: a full link needs no probe.
    pub fn probe(&mut self, now: Instant) -> Option<Probe> {
        if let Some((_, _, at)) = self.probing {
            let base = self.minima.iter().map(|m| m.1).fold(f64::INFINITY, f64::min);
            let wait = PROBE_TIMEOUT.max(Duration::from_secs_f64(if base.is_finite() { 3.0 * base / 1000.0 } else { 0.0 }));
            if now.duration_since(at) < wait {
                return None;
            }
            self.probing = None;
            self.unanswered += 1;
            self.next_probe = Some(now + PROBE_EVERY);
        }
        let ceiling = self.cfg.max_bps / HEADROOM;
        let retry = self.before_drop.filter(|&(p, at)| now.duration_since(at) < BASE_WINDOW && self.link.is_some_and(|l| p > l));
        let wanted = match self.link {
            Some(l) => l < ceiling && (retry.is_some() || self.rate >= 0.98 * self.top()),
            None => true,
        };
        if !(self.warm && self.calm && wanted && self.unanswered < MAX_UNANSWERED && self.next_probe.is_none_or(|t| now >= t)) {
            return None;
        }
        let bps = match self.link {
            Some(l) => (PROBE_GAIN * l).max(retry.map_or(0.0, |r| r.0)),
            None => (STARTUP_GAIN * self.shown).max(FIRST_PROBE),
        }
        .min(ceiling);
        self.probe_id = self.probe_id.wrapping_add(1);
        self.probing = Some((self.probe_id, bps, now));
        Some(Probe { id: self.probe_id, bps })
    }

    /// The viewer's answer to probe `id`: `sent` bits/s went out while it ran
    /// (frames and padding), `got` arrived. `own_limited`: the sender's own
    /// queue held some padding back, so a shortfall may be ours.
    pub fn on_probe(&mut self, now: Instant, id: u16, sent: f64, got: f64, own_limited: bool) {
        let Some((pid, bps, _)) = self.probing else { return };
        if pid != id {
            return;
        }
        self.probing = None;
        self.unanswered = 0;
        let retried = self.before_drop.take().is_some();
        // More arriving than was sent is a queue draining (a phone hotspot
        // catching up after a hiccup read 22 Mbps on a 0.7 Mbps probe):
        // the probe shows only what it sent.
        let got = got.min(sent);
        let full = got >= PROBE_FULL * sent;
        let capped = bps >= 0.999 * self.cfg.max_bps / HEADROOM;
        let before = self.link;
        match self.link {
            // One probe reads a bursty link low as easily as high (a phone
            // hotspot delivers twice its average in some 100 ms): a lower
            // reading moves the link halfway and is checked again soon.
            // Frames that back up measure a real drop anyway.
            Some(l) if !full && !own_limited && got < l => self.link = Some(0.5 * (l + got)),
            _ if !full && !own_limited => self.link = Some(got),
            Some(l) => self.link = Some(l.max(got)),
            None if full && capped => self.link = Some(got),
            None => self.shown = self.shown.max(got),
        }
        // Again soon while probes get everything through or read the link
        // higher than before, or read it well under what it was.
        let again = match before {
            Some(l) => (full || got > 1.05 * l) && !capped || got < 0.8 * l,
            None => full && !capped,
        };
        self.next_probe = Some(now + if again { PROBE_AGAIN } else { PROBE_EVERY });
        // A retry that got more through than frames read: the drop was a
        // hiccup, and the rate cut for it goes back at once.
        if retried && before.is_some_and(|l| got > l) {
            self.rate = self.rate.max(HEADROOM * got);
        }
        self.rate = self.rate.min(self.top()).max(self.cfg.min_bps);
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
        // But a sender that kept finding its queue full kept the link busy, and
        // then the average over the second is what the link carried: a link that
        // delivers in bursts (phone hotspot 2026-11-24: the best 100 ms about
        // twice the average) would otherwise look twice as fast as it is.
        let dt = now.duration_since(self.last_update).as_secs_f64().max(1e-3);
        self.last_update = now;
        self.burst_pct = (self.burst_pct - BURST_DECAY * dt).max(0.0);
        self.run = (self.run - RUN_DECAY * dt).max(0.0);
        let bits = std::mem::take(&mut self.acked_bytes) as f64 * 8.0;
        let offered = std::mem::take(&mut self.offered_bytes) as f64 * 8.0;
        if bits > 0.0 && self.first_ack.is_none() {
            self.first_ack = Some(now);
        }
        // The link is judged once acks have come back for `WARMUP`, and for
        // two round trips: until then delivery shows the round trip, not the
        // link. A session starts on a relay (phone hotspot 2026-11-24: 1 s
        // round trip) and the first second delivered next to nothing, which
        // cut the rate from 30 to 1.5 Mbps before the direct path was up.
        let settle = WARMUP.max(Duration::from_secs_f64(if base.is_finite() { 2.0 * base / 1000.0 } else { 0.0 }));
        let warm = self.first_ack.is_some_and(|t| now.duration_since(t) >= settle);
        self.slots.push_back(Slot { delivered: bits / dt, chosen: self.rate, offered: offered / dt, backlog: self.backlog });
        while self.slots.len() > if warm { SLOTS } else { 1 } {
            self.slots.pop_front();
        }
        let backlogged = self.slots.iter().filter(|s| s.backlog).count();
        let saturated = backlogged >= SATURATED;
        // (delivered, chosen, offered) on average from slot `from` on.
        let avg = |from: usize| {
            let n = (self.slots.len() - from) as f64;
            let sum = |f: fn(&Slot) -> f64| self.slots.iter().skip(from).map(f).sum::<f64>() / n;
            (sum(|s| s.delivered), sum(|s| s.chosen), sum(|s| s.offered))
        };
        // A saturated link is judged over the second or from the first backlog
        // on, whichever got more through: lighter frames before that only show
        // the picture, but a backlog that a WiFi stall started counts that
        // stall in a short span.
        let all = avg(0);
        let (mean, chosen, demand) = match self.slots.iter().position(|s| s.backlog) {
            Some(first) if saturated && first > 0 => {
                let busy = avg(first);
                if busy.0 > all.0 { busy } else { all }
            }
            _ => all,
        };
        // Nothing at all got through for a while: the path stalled or died
        // (a laptop moving from a phone hotspot to WiFi, 2026-11-24: 4.5 s
        // without a frame). Cutting then only leaves a new path at the
        // lowest rate (it took 12 s to climb back), so the rate and the
        // link's rate wait for delivery to judge by. Even a link far too
        // slow for the rate acks a frame within that time: the transport
        // queues little.
        let stalled = self.slots.len() >= STALL
            && self.slots.iter().rev().take(STALL).all(|s| s.delivered == 0.0)
            && self.slots.iter().rev().take(STALL).any(|s| s.offered > 0.0);
        // Measured only while the sender is still backed up: once a cut has
        // let the queue drain, the second's later slots show the rate, not
        // the link. Taken over the slots that had something to send (a
        // backlog, or at least `APP_LIMITED` of the rate offered): motion
        // after a still screen started a backlog in a second that was mostly
        // still, and over all of it the link read 0.7 Mbps on a hotspot
        // carrying about 5 (2026-11-24). Not just from the first backlog on:
        // that can hold a burst that drained a queue built before it (a link
        // alternating 1.8 and 0.2 times its rate read 1.27 times it and held
        // the rate there). Also by a second without any backlog that got more
        // through, whose delivery holds no queue drained in a burst.
        let loaded = {
            let busy = |s: &&Slot| s.backlog || s.offered >= APP_LIMITED * s.chosen;
            let n = self.slots.iter().filter(busy).count();
            if n == 0 { all.0 } else { self.slots.iter().filter(busy).map(|s| s.delivered).sum::<f64>() / n as f64 }
        };
        if saturated && self.backlog && !stalled {
            self.lower_link(now, loaded);
        } else if backlogged == 0
            && let Some(l) = self.link
            && all.0 > l
        {
            self.link = Some(all.0);
        }
        self.warm = warm;
        self.calm = backlogged == 0 && !stalled;
        self.delivery = if saturated { mean } else { self.slots.iter().map(|s| s.delivered).fold(0.0, f64::max) };
        // A backlog or frames failing judge the link whatever the load: a
        // skipped frame is offered at its share of the highest frame rate, so
        // a screen changing 26 times a second offers under `APP_LIMITED` even
        // with every frame skipped (phone hotspot 2026-11-23: all 26 skipped
        // for seconds at 42.4 Mbps, no cut).
        let judged = warm && (demand >= APP_LIMITED * chosen || self.backlog || self.failed > 2);
        let limited = judged && self.delivery < KEEPING_UP * demand;
        let overloaded = judged && self.delivery < OVERLOADED * demand;

        let failing = self.failed > 2 && limited;
        // Frames that wait long while less gets through than was offered:
        // the transport's window holds a queue in the link, out of the
        // sender's sight, and what got through is what the link carries.
        // Delay with everything delivered is WiFi, not the link (above).
        let standing = judged && !stalled && self.queue_ms >= STANDING_QUEUE_MS && all.0 < KEEPING_UP * all.2;
        if standing {
            self.lower_link(now, all.0);
        }
        // A saturated link that still backs up is carrying all it can: even a
        // rate within `KEEPING_UP` of it keeps the queue from ever draining.
        let cut = !stalled && (standing || overloaded || failing || (self.backlog && (limited || (warm && saturated))));
        if cut {
            if now >= self.next_cut {
                // A saturated link has shown what it carries: the rates since
                // the first backlog offered `demand` and got `mean` through, so
                // aim a bit under that, unless the rate is already there (those
                // slots still hold what was offered before the last cut).
                let to = if saturated && demand > 0.0 {
                    let fit = chosen * FIT * mean / demand;
                    (fit < self.rate).then_some(fit)
                } else {
                    Some(self.rate * if overloaded || failing { 0.7 } else { 0.85 })
                };
                if let Some(to) = to {
                    self.rate = to.max(self.rate * MAX_CUT);
                    self.next_cut = now + MIN_CUT_GAP;
                }
            }
            self.hold_until = now + HOLD_AFTER_CUT;
        } else if now >= self.hold_until && !limited && !stalled && backlogged == 0 {
            self.rate *= GROWTH;
        }
        self.rate = self.rate.min(self.top()).max(self.cfg.min_bps);
        self.failed = 0;
        self.backlog = false;
        self.rate
    }

    pub fn bitrate(&self) -> f64 {
        self.rate
    }

    /// Delivered bits per second the rate is judged by: the best 100 ms slot
    /// over the last second, or the average while the link is saturated.
    pub fn delivery(&self) -> f64 {
        self.delivery
    }

    /// What the link carries, bits/s, as last measured; `None` before that.
    pub fn link(&self) -> Option<f64> {
        self.link
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

    /// The fewest parity shards a frame gets (see `MIN_PARITY`).
    pub fn min_parity(&self) -> u16 {
        MIN_PARITY.max(self.run.ceil() as u16)
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
        // A probe gets through what the link carries, at once.
        if let Some(p) = c.probe(*t) {
            c.on_probe(*t, p.id, p.bps, p.bps.min(cap), false);
        }
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
    fn runs_of_lost_shards_raise_the_parity_of_small_frames() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        assert_eq!(c.min_parity(), MIN_PARITY);
        c.on_report(&Report { shards_expected: 100, shards_received: 96, frames_recovered: 1, max_run: 4, ..Default::default() });
        assert_eq!(c.min_parity(), 4);
        c.on_report(&Report { max_run: 50, ..Default::default() });
        assert_eq!(c.min_parity(), RUN_MAX as u16);
        // Without more runs it fades back within a minute or two.
        run(&mut c, &mut t, 300, 5.0, 100.0 * MBPS);
        assert!(c.min_parity() < RUN_MAX as u16, "{}", c.min_parity());
        run(&mut c, &mut t, 600, 5.0, 100.0 * MBPS);
        assert_eq!(c.min_parity(), MIN_PARITY);
        assert_eq!(c.bitrate(), 20.0 * MBPS, "runs are loss, not congestion");
    }

    #[test]
    fn runs_lost_while_queueing_add_no_parity() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 20, 5.0, 100.0 * MBPS);
        run(&mut c, &mut t, 5, 5.0, 4.0 * MBPS);
        c.on_backlog();
        c.on_report(&Report { shards_expected: 100, shards_received: 90, max_run: 6, ..Default::default() });
        assert_eq!(c.min_parity(), MIN_PARITY);
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
    fn a_new_highest_rate_is_tried_at_once_and_a_lower_one_caps() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        c.set_max(40.0 * MBPS);
        assert_eq!(c.bitrate(), 40.0 * MBPS, "nothing known about the link yet");
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        run(&mut c, &mut t, 100, 5.0, 8.0 * MBPS);
        let low = c.bitrate();
        assert!(low < 10.0 * MBPS, "{low}");
        c.set_max(15.0 * MBPS);
        assert_eq!(c.bitrate(), low, "lower than before: the link's rate stays");
        c.set_max(40.0 * MBPS);
        assert_eq!(c.bitrate(), (HEADROOM * c.link().unwrap()).max(low), "only up to what the link has shown");
        c.set_max(5.0 * MBPS);
        assert_eq!(c.bitrate(), 5.0 * MBPS);
        run(&mut c, &mut t, 50, 5.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 5.0 * MBPS, "never above the new highest");
    }

    #[test]
    fn never_below_min() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        for _ in 0..100 {
            let bytes = (c.bitrate() / 8.0 / 60.0) as u64 * 18;
            c.on_frame_offered(bytes);
            c.on_frame_acked(5.0, bytes / 20);
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

    #[test]
    fn skipping_at_a_low_frame_rate_cuts() {
        // A screen changing 20 times a second; each frame is due at its share of 120 fps.
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 40.0 * MBPS), t);
        let share = |c: &RateController| (c.bitrate() / 8.0 / 120.0) as u64;
        for _ in 0..10 {
            t += Duration::from_millis(100);
            for _ in 0..2 {
                c.on_frame_offered(share(&c));
                c.on_frame_acked(5.0, share(&c));
            }
            c.update(t);
        }
        assert_eq!(c.bitrate(), 40.0 * MBPS, "everything delivered: not judged");
        // The link slows to a crawl: almost every frame is skipped behind a backlog.
        for _ in 0..20 {
            t += Duration::from_millis(100);
            for _ in 0..2 {
                c.on_frame_offered(share(&c));
                c.on_backlog();
            }
            c.on_frame_acked(5.0, share(&c) / 10);
            c.update(t);
        }
        assert!(c.bitrate() < 10.0 * MBPS, "{}", c.bitrate());
    }

    /// Results of `link_run`, over its second half.
    #[derive(Debug)]
    struct Run {
        /// Delivered over what the link carries on average.
        used: f64,
        /// Frames skipped, of all frames due.
        skipped: f64,
        /// Mean time data waited at the sender and in the link, ms.
        wait_ms: f64,
        /// Longest any frame waited, ms.
        worst_wait_ms: f64,
        /// Most frames skipped in one second.
        worst_skips: usize,
    }

    /// A probe in `link_run`.
    struct SimProbe {
        p: Probe,
        /// Frame steps of padding left, and done.
        sending: u32,
        sent_steps: u32,
        /// Frame steps since it started.
        steps: u32,
        /// Bits sent while padding, and arrived until the last padding did.
        sent: f64,
        got: f64,
        /// Padding was held back by the sender's own queue.
        held: bool,
        /// Queue delay when it started, ms.
        queue_ms: f64,
    }

    /// A sender at 60 fps behind a link that carries `cap` bits/s on average,
    /// in bursts: in tick `i` (100 ms) it carries `cap * bursts[i % len]`,
    /// 45 ms away. The transport keeps up to `window(c)` bits in flight; the
    /// rest waits at the sender, which skips a frame while more than one
    /// frame waits there (`WHOLE_FRAMES`). Frames come out `content(i, rate)` times
    /// the size the rate gives them, and a skipped frame is offered at that
    /// size too. Probes send for `PROBE_STEPS` frames as `video_tx` does:
    /// padding up to the probe's rate (at least a quarter of it) while under
    /// half a frame waits at the sender, a window for the probe's rate, and
    /// skips not counted as backlog until a tick after.
    fn link_run(
        c: &mut RateController,
        t: &mut Instant,
        ticks: u32,
        cap: f64,
        bursts: &[f64],
        content: &dyn Fn(u32, f64) -> f64,
        window: &dyn Fn(&RateController) -> f64,
    ) -> Run {
        const FPS: f64 = 60.0;
        const BASE: f64 = 0.045;
        const PROBE_STEPS: u32 = 12;
        let (mut own, mut net_v, mut net_p) = (0.0f64, 0.0f64, 0.0f64);
        let (mut delivered, mut due, mut skipped, mut wait, mut samples) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut worst_wait, mut worst_skips, mut recent) = (0.0f64, 0usize, VecDeque::new());
        let mut probe: Option<SimProbe> = None;
        let (mut video_bps, mut quiet) = (0.0f64, 0u32);
        for i in 0..ticks {
            let carry = cap * bursts[i as usize % bursts.len()] / FPS;
            let measure = i >= ticks / 2;
            let mut acked = 0.0;
            let mut skips = 0;
            let mut moved_tick = 0.0;
            for _ in 0..6 {
                let normal = c.bitrate() / FPS;
                let frame = normal * content(i, c.bitrate());
                if own > normal {
                    if quiet == 0 {
                        c.on_backlog();
                    }
                    skips += 1;
                } else {
                    own += frame;
                }
                c.on_frame_offered((frame / 8.0) as u64);
                // What is in flight beyond the path's own round trip queues;
                // a window under that round trip limits the rate instead.
                let w = probe.as_ref().map_or(window(c), |p| window(c).max(p.p.bps * 0.125));
                let out_max = carry.min(w / BASE / FPS);
                let wire = c.bitrate().min(cap) * BASE;
                let net = net_v + net_p;
                let moved = own.min(((w - wire).max(0.0) + out_max - net).max(0.0));
                own -= moved;
                net_v += moved;
                moved_tick += moved;
                let queue_ms = (own + net_v + net_p) / cap * 1000.0;
                if let Some(p) = probe.as_mut()
                    && p.sending > 0
                {
                    // The sender stops early once the round trip has grown by
                    // `PROBE_ABORT_MS` (seen at once here).
                    if queue_ms - p.queue_ms > PROBE_ABORT_MS {
                        p.sending = 0;
                    } else {
                        p.sending -= 1;
                        p.sent_steps += 1;
                        p.sent += moved;
                        let pad = (p.p.bps - video_bps).max(0.25 * p.p.bps) / FPS;
                        if own <= 0.5 * normal {
                            net_p += pad;
                            p.sent += pad;
                        } else {
                            p.held = true;
                        }
                    }
                }
                let net = net_v + net_p;
                let out = net.min(out_max);
                let out_v = if net > 0.0 { out * net_v / net } else { 0.0 };
                net_v -= out_v;
                net_p -= out - out_v;
                acked += out_v;
                // The viewer counts what arrives until the last padding has.
                if let Some(p) = probe.as_mut() {
                    p.steps += 1;
                    p.got += out;
                    if p.sending == 0 && net_p < 1.0 {
                        let sent = p.sent / (p.sent_steps.max(1) as f64 / FPS);
                        c.on_probe(*t, p.p.id, sent, p.got / (p.steps as f64 / FPS), p.held);
                        probe = None;
                    }
                }
                let waited = (own + net_v + net_p) / cap * 1000.0;
                c.on_frame_acked(BASE * 1000.0 + waited, 0);
                if measure {
                    due += 1.0;
                    delivered += out_v;
                    wait += waited;
                    samples += 1.0;
                    worst_wait = worst_wait.max(waited);
                }
            }
            video_bps = moved_tick * 10.0;
            if measure {
                skipped += skips as f64;
                recent.push_back(skips);
                if recent.len() > 10 {
                    recent.pop_front();
                }
                worst_skips = worst_skips.max(recent.iter().sum());
            }
            c.on_frame_acked_late((acked / 8.0) as u32);
            *t += Duration::from_millis(100);
            c.update(*t);
            quiet = quiet.saturating_sub(1);
            if probe.is_none()
                && let Some(p) = c.probe(*t)
            {
                let queue_ms = (own + net_v + net_p) / cap * 1000.0;
                probe = Some(SimProbe { p, sending: PROBE_STEPS, sent_steps: 0, steps: 0, sent: 0.0, got: 0.0, held: false, queue_ms });
                quiet = PROBE_STEPS / 6 + 1;
            }
        }
        Run {
            used: delivered / (cap * (ticks - ticks / 2) as f64 / 10.0),
            skipped: skipped / due,
            wait_ms: wait / samples,
            worst_wait_ms: worst_wait,
            worst_skips,
        }
    }

    const LINK: f64 = 16.0 * MBPS;

    /// The transport's window (`cc`): the chosen rate for 45 ms + 80 ms.
    fn our_window(c: &RateController) -> f64 {
        c.bitrate() * 0.125
    }

    /// BBR's own window on a phone hotspot (2026-11-24): its estimate 1.8
    /// times what the link carried, for 45 ms + 80 ms.
    fn bbr_window(_: &RateController) -> f64 {
        LINK * 1.8 * 0.125
    }

    fn session(bursts: &[f64], content: &dyn Fn(u32, f64) -> f64, window: &dyn Fn(&RateController) -> f64) -> Run {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 53.0 * MBPS), t);
        link_run(&mut c, &mut t, 400, LINK, bursts, content, window)
    }

    #[test]
    fn heavy_motion_keeps_the_queue_short() {
        // A steady link, one carrying 1.8 and 0.2 times its rate in turns of
        // 100 ms, and WiFi stalling for 100 ms every 3 s. With the rate under
        // the link, even BBR's oversized window (`bbr_window`) queues little.
        let stalls: Vec<f64> = [1.0; 29].into_iter().chain([0.0]).collect();
        // The alternating link reads low in probes (see below): it is used less.
        for (name, bursts, used) in [("steady", &[1.0][..], 0.8), ("bursty", &[1.8, 0.2][..], 0.75), ("stalls", &stalls[..], 0.8)] {
            let r = session(bursts, &|_, _| 1.0, &our_window);
            assert!(r.used > used && r.skipped < 0.05 && r.wait_ms < 80.0 && r.worst_wait_ms < 140.0, "{name}: {r:?}");
            let bbr = session(bursts, &|_, _| 1.0, &bbr_window);
            assert!(bbr.used > used - 0.1 && bbr.wait_ms < 80.0 && bbr.worst_wait_ms < 160.0, "{name}: BBR's window {bbr:?}");
        }
    }

    #[test]
    fn motion_after_a_still_desktop_fits_the_link() {
        for bursts in [&[1.0][..], &[1.8, 0.2][..]] {
            // Frames at 40% of the rate leave it near what the link carried...
            let mut t = Instant::now();
            let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 53.0 * MBPS), t);
            link_run(&mut c, &mut t, 200, LINK, bursts, &|_, _| 0.4, &our_window);
            assert!(c.bitrate() <= LINK, "{}", c.bitrate());
            // ...so the first second of full frames after 20 s of light ones skips few.
            let r = session(bursts, &|i, _| if i < 200 { 0.4 } else { 1.0 }, &our_window);
            assert!(r.worst_skips <= 12 && r.worst_wait_ms < 180.0, "{r:?}");
        }
    }

    #[test]
    fn a_stall_keeps_the_rate_for_the_path_after_it() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 53.0 * MBPS), t);
        run(&mut c, &mut t, 50, 5.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 53.0 * MBPS);
        // The laptop changes networks: the last frames trickle in, then for
        // 4.5 s nothing is delivered, frames are skipped and time out.
        for i in 0..45 {
            t += Duration::from_millis(100);
            for _ in 0..6 {
                c.on_frame_offered((c.bitrate() / 8.0 / 60.0) as u64);
                c.on_backlog();
            }
            if i == 0 {
                c.on_frame_acked(50.0, 5_000);
            } else {
                c.on_frame_failed();
            }
            c.update(t);
        }
        assert!(c.bitrate() >= 0.5 * 53.0 * MBPS, "{}", c.bitrate());
        c.new_path();
        run(&mut c, &mut t, 20, 2.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 53.0 * MBPS);
    }

    #[test]
    fn the_rate_stays_near_what_the_link_carried() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 40.0 * MBPS), t);
        run(&mut c, &mut t, 100, 5.0, 8.0 * MBPS);
        let link = c.link().unwrap();
        assert!((7.0 * MBPS..=8.5 * MBPS).contains(&link), "{link}");
        // Frames at 40% of the rate all get through but show nothing about
        // the rest: the rate stays under `PROBE` times the link.
        for _ in 0..100 {
            t += Duration::from_millis(100);
            for _ in 0..6 {
                let bytes = (c.bitrate() / 8.0 / 60.0 * 0.4) as u64;
                c.on_frame_offered(bytes);
                c.on_frame_acked(5.0, bytes);
            }
            c.update(t);
        }
        assert!(c.bitrate() <= HEADROOM * link * 1.001, "{}", c.bitrate());
        // The link gets faster and full frames get through: the rate follows them up.
        run(&mut c, &mut t, 300, 5.0, 100.0 * MBPS);
        assert_eq!(c.bitrate(), 40.0 * MBPS);
    }

    #[test]
    fn light_frames_still_find_a_faster_link() {
        // 2 s of full frames on a link carrying 2 Mbps, then light frames on
        // 8 Mbps: they show nothing about the link, but the probes do...
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 53.0 * MBPS), t);
        link_run(&mut c, &mut t, 20, 2.0 * MBPS, &[1.0], &|_, _| 1.0, &our_window);
        assert!(c.bitrate() < 2.0 * MBPS, "{}", c.bitrate());
        let r = link_run(&mut c, &mut t, 200, 8.0 * MBPS, &[1.0], &|_, _| 0.45, &our_window);
        assert!(c.bitrate() > 0.7 * 8.0 * MBPS, "{}", c.bitrate());
        assert!(r.worst_wait_ms < 80.0, "probes queue little: {r:?}");
        // ...so full frames fit at once.
        let r = link_run(&mut c, &mut t, 80, 8.0 * MBPS, &[1.0], &|_, _| 1.0, &our_window);
        assert!(r.skipped < 0.05 && r.wait_ms < 60.0, "{r:?}");
    }

    #[test]
    fn motion_that_fits_raises_a_low_reading() {
        // A messy start reads the link at 3 Mbps; it carries 8. Motion that
        // fills 55% of the rate never backs up, but the rate still climbs.
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
        link_run(&mut c, &mut t, 30, 3.0 * MBPS, &[1.0], &|_, _| 1.0, &our_window);
        assert!(c.bitrate() < 4.0 * MBPS, "{}", c.bitrate());
        let r = link_run(&mut c, &mut t, 300, 8.0 * MBPS, &[1.0], &|_, _| 0.55, &our_window);
        assert!(c.bitrate() > 6.5 * MBPS, "{}", c.bitrate());
        assert!(r.skipped < 0.05 && r.wait_ms < 60.0, "{r:?}");
        // Full frames then fit the link again within about a second.
        let r = link_run(&mut c, &mut t, 100, 8.0 * MBPS, &[1.0], &|_, _| 1.0, &our_window);
        assert!(r.skipped < 0.05 && r.wait_ms < 80.0, "{r:?}");
    }

    #[test]
    fn a_still_start_finds_the_link_before_motion() {
        // Phone hotspot 2026-11-24: 16 s of still screen at 30 Mbps, then
        // motion on a link carrying about 5 skipped frames for seconds. The
        // probes find the link while the screen is still. A link alternating
        // 1.8 and 0.2 times its rate reads lower: a probe at its average
        // queues there.
        for (bursts, least) in [(&[1.0][..], 0.8), (&[1.8, 0.2][..], 0.55)] {
            let mut t = Instant::now();
            let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
            let r = link_run(&mut c, &mut t, 100, 5.0 * MBPS, bursts, &|_, _| 0.02, &our_window);
            let link = c.link().unwrap_or(0.0);
            assert!((least * 5.0 * MBPS..=6.0 * MBPS).contains(&link), "{bursts:?}: {link}");
            assert!(c.bitrate() <= 5.0 * MBPS && r.worst_wait_ms < 80.0, "{bursts:?}: {} {r:?}", c.bitrate());
            let r = link_run(&mut c, &mut t, 100, 5.0 * MBPS, bursts, &|_, _| 1.0, &our_window);
            assert!(c.bitrate() > 0.5 * 5.0 * MBPS, "{bursts:?}: {}", c.bitrate());
            assert!(r.skipped < 0.05 && r.wait_ms < 80.0 && r.worst_skips <= 6, "{bursts:?}: {r:?}");
        }
    }

    #[test]
    fn probes_stop_when_the_viewer_never_answers() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
        let mut probes = 0;
        for _ in 0..600 {
            t += Duration::from_millis(100);
            c.on_frame_offered(1_000);
            c.on_frame_acked(20.0, 1_000);
            c.update(t);
            probes += c.probe(t).is_some() as u32;
        }
        assert_eq!(probes, MAX_UNANSWERED);
        assert_eq!(c.bitrate(), 30.0 * MBPS, "nothing measured: the rate is left alone");
    }

    #[test]
    fn late_ack_counts_delivery_without_polluting_rtt() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(2.0 * MBPS, 20.0 * MBPS), t);
        t += Duration::from_millis(100);
        c.on_frame_offered(10_000);
        c.on_frame_acked(5.0, 5_000);
        c.on_frame_acked_late(5_000);
        c.update(t);
        assert_eq!(c.queue_ms(), 0.0);
        assert!((c.delivery() - 10_000.0 * 8.0 / 0.1).abs() < 1e-6);
    }

    #[test]
    fn frames_over_budget_keep_the_queue_short() {
        // Intel QSV on a phone hotspot (2026-11-24): at a few Mbps a moving
        // 1440p screen still makes frames of 12-77 KB, 3-14 times the budget.
        // The rate cannot shrink them; skipping frames keeps the queue short.
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 53.0 * MBPS), t);
        let r = link_run(&mut c, &mut t, 400, 4.0 * MBPS, &[1.0], &|_, rate| (12_000.0 * 8.0 * 60.0 / rate).max(1.0), &our_window);
        assert!(r.used > 0.9 && r.wait_ms < 60.0 && r.worst_wait_ms < 100.0, "{r:?}");
    }

    #[test]
    fn hiccups_do_not_throw_the_link_away() {
        // Phone hotspot 2026-11-24: every 10 s or so the link carries next
        // to nothing for 400 ms, then catches up in a burst. The link
        // reading fell from 8 to 0.1 Mbps each time and took 10-25 s to
        // climb back, skipping most frames meanwhile.
        let hiccup: Vec<f64> = [1.0; 100].into_iter().chain([0.05; 4]).chain([1.8, 1.8]).collect();
        for content in [1.0, 0.5] {
            let mut t = Instant::now();
            let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
            link_run(&mut c, &mut t, 100, 8.0 * MBPS, &[1.0], &|_, _| content, &our_window);
            let r = link_run(&mut c, &mut t, 600, 8.0 * MBPS, &hiccup, &|_, _| content, &our_window);
            let link = c.link().unwrap_or(0.0) / MBPS;
            assert!(link > 0.8 * 8.0, "{content}: {link} {r:?}");
            assert!(r.used > 0.7 * content && r.skipped < 0.08, "{content}: {r:?}");
        }
    }

    #[test]
    fn a_faster_link_is_found_within_seconds() {
        // netshape 2026-11-24: 3 then 6 Mbps; the rate stayed near 2.5 for
        // 10 s, each probe reading a little higher and the next 5 s away.
        // Now the first probe after the change (within `PROBE_EVERY`) starts
        // a climb by `PROBE_GAIN` every `PROBE_AGAIN`.
        for content in [1.0, 0.5] {
            let mut t = Instant::now();
            let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
            link_run(&mut c, &mut t, 100, 3.0 * MBPS, &[1.0], &|_, _| content, &our_window);
            let r = link_run(&mut c, &mut t, 80, 6.0 * MBPS, &[1.0], &|_, _| content, &our_window);
            let link = c.link().unwrap_or(0.0) / MBPS;
            assert!(link > 0.85 * 6.0, "{content}: {link} {r:?}");
        }
    }

    #[test]
    fn a_probe_reads_no_more_than_it_sent() {
        let mut t = Instant::now();
        let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
        run(&mut c, &mut t, 100, 5.0, 8.0 * MBPS);
        let link = c.link().unwrap();
        // A queue drains into a probe that could send little.
        let mut answered = false;
        for _ in 0..200 {
            t += Duration::from_millis(100);
            for _ in 0..6 {
                let bytes = (c.bitrate() / 8.0 / 60.0) as u64;
                c.on_frame_offered(bytes);
                c.on_frame_acked(5.0, bytes);
            }
            c.update(t);
            if let Some(p) = c.probe(t) {
                c.on_probe(t, p.id, 0.7 * MBPS, 22.0 * MBPS, true);
                answered = true;
                break;
            }
        }
        assert!(answered);
        assert!(c.link().unwrap() <= link.max(0.7 * MBPS) * 1.001, "{:?} {link}", c.link());
    }

    #[test]
    fn a_changing_link_is_followed_whatever_the_picture() {
        // A phone hotspot's upload moving between 3 and 11 Mbps every 10 s,
        // under a moving and a mostly still picture.
        for content in [1.0, 0.3] {
            let mut t = Instant::now();
            let mut c = RateController::new(RateConfig::new(1.5 * MBPS, 30.0 * MBPS), t);
            for cap in [8.0, 3.0, 11.0, 5.0, 8.0, 3.0] {
                let r = link_run(&mut c, &mut t, 100, cap * MBPS, &[1.0], &|_, _| content, &our_window);
                let link = c.link().unwrap_or(0.0) / MBPS;
                assert!((0.9 * cap..=1.1 * cap).contains(&link), "{content} at {cap}: {link} {r:?}");
                assert!(r.skipped < 0.01 && r.worst_wait_ms < 60.0, "{content} at {cap}: {r:?}");
            }
        }
    }
}
