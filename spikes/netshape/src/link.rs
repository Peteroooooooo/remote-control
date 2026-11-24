//! A bottleneck link: a tail-drop buffer served at a rate that follows a
//! schedule and has hiccups, then a fixed delay.

use std::{collections::VecDeque, time::Duration};

#[derive(Clone, Debug)]
pub struct LinkConfig {
    /// (bits/s, how long), cycling. One entry for a fixed rate.
    pub schedule: Vec<(f64, Duration)>,
    /// Buffer size, as time at the nominal rate.
    pub queue: Duration,
    pub delay: Duration,
    /// Chance per packet (0..=1) that a loss event starts, and its length.
    pub loss: f64,
    pub loss_burst: u32,
    /// Hiccups: about every `hiccup_every` (±20%) the link serves
    /// `hiccup_factor` times its rate for `hiccup`, then `catchup_factor`
    /// times it for as long again.
    pub hiccup_every: Option<Duration>,
    pub hiccup: Duration,
    pub hiccup_factor: f64,
    pub catchup_factor: f64,
}

impl LinkConfig {
    #[cfg(test)]
    pub fn fixed(bps: f64) -> Self {
        LinkConfig {
            schedule: vec![(bps, Duration::from_secs(1))],
            queue: Duration::from_millis(200),
            delay: Duration::ZERO,
            loss: 0.0,
            loss_burst: 1,
            hiccup_every: None,
            hiccup: Duration::from_millis(400),
            hiccup_factor: 0.05,
            catchup_factor: 1.8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub in_pkts: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    pub dropped_queue: u64,
    pub dropped_loss: u64,
}

pub struct Link<T> {
    cfg: LinkConfig,
    rng: fastrand::Rng,
    /// Time since the start up to which the link has been served.
    now: Duration,
    queue: VecDeque<(T, usize)>,
    queued: usize,
    /// Bytes the link may still serve toward the head of the queue.
    credit: f64,
    delayed: VecDeque<(T, Duration)>,
    loss_left: u32,
    next_hiccup: Option<Duration>,
    pub counters: Counters,
}

/// Steps the fluid model is advanced in.
const SLICE: Duration = Duration::from_micros(500);

impl<T> Link<T> {
    pub fn new(cfg: LinkConfig, seed: u64) -> Self {
        let mut rng = fastrand::Rng::with_seed(seed);
        let next_hiccup = cfg.hiccup_every.map(|e| e.mul_f64(rng.f64() * 0.4 + 0.8));
        Link {
            cfg,
            rng,
            now: Duration::ZERO,
            queue: VecDeque::new(),
            queued: 0,
            credit: 0.0,
            delayed: VecDeque::new(),
            loss_left: 0,
            next_hiccup,
            counters: Counters::default(),
        }
    }

    /// The schedule's rate at `t`, bits/s.
    pub fn nominal(&self, t: Duration) -> f64 {
        let cycle: Duration = self.cfg.schedule.iter().map(|s| s.1).sum();
        let mut at = Duration::from_secs_f64(t.as_secs_f64() % cycle.as_secs_f64().max(1e-9));
        for &(bps, len) in &self.cfg.schedule {
            if at < len {
                return bps;
            }
            at -= len;
        }
        self.cfg.schedule[0].0
    }

    /// The share of the nominal rate served at `t`, moving the hiccup
    /// schedule on once one is over.
    fn factor(&mut self, t: Duration) -> f64 {
        let Some(start) = self.next_hiccup else { return 1.0 };
        let h = self.cfg.hiccup;
        if t < start {
            1.0
        } else if t < start + h {
            self.cfg.hiccup_factor
        } else if t < start + 2 * h {
            self.cfg.catchup_factor
        } else {
            let every = self.cfg.hiccup_every.unwrap_or_default();
            self.next_hiccup = Some(start + 2 * h + every.mul_f64(self.rng.f64() * 0.4 + 0.8));
            1.0
        }
    }

    pub fn in_hiccup(&self) -> bool {
        self.next_hiccup.is_some_and(|s| self.now >= s && self.now < s + self.cfg.hiccup)
    }

    /// Queue depth as time at the nominal rate.
    pub fn queue_ms(&self) -> f64 {
        self.queued as f64 * 8.0 / self.nominal(self.now) * 1000.0
    }

    fn advance(&mut self, to: Duration) {
        while self.now < to {
            let step = SLICE.min(to - self.now);
            let mid = self.now + step / 2;
            let bps = self.nominal(mid) * self.factor(mid);
            self.now += step;
            if self.queue.is_empty() {
                self.credit = 0.0;
                continue;
            }
            self.credit += bps * step.as_secs_f64() / 8.0;
            while let Some(&(_, len)) = self.queue.front() {
                if (len as f64) > self.credit {
                    break;
                }
                let (item, len) = self.queue.pop_front().unwrap();
                self.credit -= len as f64;
                self.queued -= len;
                self.counters.out_bytes += len as u64;
                self.delayed.push_back((item, self.now + self.cfg.delay));
            }
            if self.queue.is_empty() {
                self.credit = 0.0;
            }
        }
    }

    /// A packet of `len` bytes arrives at `t`. False if it was dropped.
    pub fn push(&mut self, t: Duration, item: T, len: usize) -> bool {
        self.advance(t);
        self.counters.in_pkts += 1;
        self.counters.in_bytes += len as u64;
        if self.loss_left > 0 {
            self.loss_left -= 1;
            self.counters.dropped_loss += 1;
            return false;
        }
        if self.cfg.loss > 0.0 && self.rng.f64() < self.cfg.loss {
            self.loss_left = self.cfg.loss_burst.saturating_sub(1);
            self.counters.dropped_loss += 1;
            return false;
        }
        let limit = self.nominal(t) * self.cfg.queue.as_secs_f64() / 8.0;
        if (self.queued + len) as f64 > limit {
            self.counters.dropped_queue += 1;
            return false;
        }
        self.queued += len;
        self.queue.push_back((item, len));
        true
    }

    /// Packets that have left the link by `t`, in order.
    pub fn pop_ready(&mut self, t: Duration) -> Vec<T> {
        self.advance(t);
        let mut out = Vec::new();
        while self.delayed.front().is_some_and(|d| d.1 <= t) {
            out.push(self.delayed.pop_front().unwrap().0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// Offers a 1200-byte packet every `gap` until `until`, collecting what
    /// comes out each ms: (id, time out).
    fn drive(link: &mut Link<u64>, gap: Duration, until: Duration) -> Vec<(u64, Duration, Duration)> {
        let mut sent = Vec::new();
        let mut out = Vec::new();
        let (mut t, mut next, mut id) = (Duration::ZERO, Duration::ZERO, 0u64);
        while t < until + Duration::from_secs(2) {
            if t >= next && t < until {
                link.push(t, id, 1200);
                sent.push(t);
                id += 1;
                next += gap;
            }
            for i in link.pop_ready(t) {
                out.push((i, sent[i as usize], t));
            }
            t += Duration::from_micros(100);
        }
        out
    }

    #[test]
    fn over_capacity_serves_the_rate_and_drops_the_rest() {
        let mut l = Link::new(LinkConfig::fixed(8e6), 1);
        let out = drive(&mut l, MS, Duration::from_secs(5));
        let mbps = out.iter().filter(|o| o.2 <= Duration::from_secs(5)).count() as f64 * 1200.0 * 8.0 / 5.0 / 1e6;
        assert!((mbps - 8.0).abs() < 0.24, "{mbps}");
        assert!(l.counters.dropped_queue > 500, "{:?}", l.counters);
        let worst = out.iter().map(|o| o.2 - o.1).max().unwrap();
        assert!(worst <= Duration::from_millis(202), "{worst:?}");
    }

    #[test]
    fn under_capacity_everything_arrives_after_its_serialisation_and_the_delay() {
        let mut cfg = LinkConfig::fixed(8e6);
        cfg.delay = Duration::from_millis(30);
        let mut l = Link::new(cfg, 1);
        let out = drive(&mut l, Duration::from_micros(2400), Duration::from_secs(3));
        assert_eq!(out.len() as u64, l.counters.in_pkts);
        assert_eq!(l.counters.dropped_queue + l.counters.dropped_loss, 0);
        for (_, sent, got) in &out {
            let ms = (*got - *sent).as_secs_f64() * 1000.0;
            assert!((31.0..=32.5).contains(&ms), "{ms}");
        }
    }

    #[test]
    fn a_hiccup_queues_without_dropping_then_drains() {
        let mut cfg = LinkConfig::fixed(8e6);
        cfg.queue = Duration::from_millis(600);
        cfg.hiccup_every = Some(Duration::from_secs(2));
        let mut l = Link::new(cfg, 7);
        let start = l.next_hiccup.unwrap();
        let out = drive(&mut l, Duration::from_micros(2400), start + Duration::from_secs(2));
        assert_eq!(l.counters.dropped_queue, 0);
        let during = out.iter().filter(|o| o.2 > start + 10 * MS && o.2 < start + 400 * MS).count();
        assert!(during < 20, "{during} out during the hiccup");
        // Offered 4 Mbps for 400 ms: about 200 KB, about 200 ms at 8 Mbps.
        let worst = out.iter().map(|o| o.2 - o.1).max().unwrap();
        assert!((Duration::from_millis(330)..Duration::from_millis(420)).contains(&worst), "{worst:?}");
        // Drained by the end of the catch-up.
        let late = out.iter().filter(|o| o.1 > start + 900 * MS).map(|o| o.2 - o.1).max().unwrap();
        assert!(late < 5 * MS, "{late:?}");
    }

    #[test]
    fn a_schedule_changes_the_rate() {
        let mut cfg = LinkConfig::fixed(8e6);
        cfg.schedule = vec![(8e6, Duration::from_secs(1)), (2e6, Duration::from_secs(1))];
        let l = Link::<u64>::new(cfg, 1);
        assert_eq!(l.nominal(Duration::from_millis(500)), 8e6);
        assert_eq!(l.nominal(Duration::from_millis(1500)), 2e6);
        assert_eq!(l.nominal(Duration::from_millis(2500)), 8e6);
        let mut cfg = LinkConfig::fixed(8e6);
        cfg.schedule = vec![(8e6, Duration::from_secs(1)), (2e6, Duration::from_secs(1))];
        let mut l = Link::new(cfg, 1);
        let out = drive(&mut l, MS, Duration::from_secs(2));
        let in_span = |a: u64, b: u64| out.iter().filter(|o| o.2 >= Duration::from_millis(a) && o.2 < Duration::from_millis(b)).count() as f64 * 9600.0 / 0.5 / 1e6;
        assert!((in_span(300, 800) - 8.0).abs() < 0.3, "{}", in_span(300, 800));
        assert!((in_span(1300, 1800) - 2.0).abs() < 0.2, "{}", in_span(1300, 1800));
    }

    #[test]
    fn losses_come_in_runs() {
        let mut cfg = LinkConfig::fixed(100e6);
        cfg.loss = 0.1 / 4.0;
        cfg.loss_burst = 4;
        let mut l = Link::new(cfg, 3);
        let mut lost = Vec::new();
        for i in 0..40_000u64 {
            if !l.push(Duration::from_micros(i * 100), i, 1200) {
                lost.push(i);
            }
        }
        let share = lost.len() as f64 / 40_000.0;
        assert!((0.08..0.12).contains(&share), "{share}");
        let runs = 1 + lost.windows(2).filter(|w| w[1] != w[0] + 1).count();
        let mean = lost.len() as f64 / runs as f64;
        assert!((3.5..=4.5).contains(&mean), "{mean}");
    }
}
