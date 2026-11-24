//! The QUIC congestion controller: BBR, with a floor under its window and
//! pacing rate taken from the video bitrate the sender has chosen.
//!
//! On a still screen almost nothing is sent. BBR then forgets its bandwidth
//! estimate (it keeps the best rate of its last two probing cycles, a few
//! seconds) and its window falls to 4 packets; every 5 s it also cuts the
//! window to 4 packets on purpose to measure the round trip again
//! (ProbeRTT). Measured on a test laptop 2026-11-06: after 18 s of still screen
//! the window was 5 KB and the estimate 0.1 Mbps, the first frames took
//! seconds to leave, the input round trip reached 1.3-2.7 s and the video
//! rate needed about 8 s to recover.
//!
//! The video rate controller (`rc_stream::rate`) already judges from the
//! viewer's acks what the link carries, so the transport only has to stop
//! that rate from being held back: it may always keep the chosen rate's worth
//! of data in flight for the minimum round trip plus `SLACK`. A link slower
//! than the chosen rate still fills this window; the sender then sees its
//! queue grow and the rate controller cuts. Where BBR allows more, BBR's
//! window applies, up to `ceiling`.
//!
//! The ceiling: on WiFi, noq's BBRv3 let its window grow by every byte acked,
//! about 1 MB/s at 8 Mbps of video, to 20-50 MB within a minute (every
//! test laptop session since P1-1; on loopback it stayed at the floor). A
//! window that size lets a WiFi stall fill the radio's queues instead of
//! our own short send queue, which is what skips stale frames.
//!
//! The floor itself is held to what the path has shown it carries. On a
//! phone hotspot (2026-11-23) the chosen rate climbed to 42.4 Mbps on a still
//! screen while the link carried 2-7 Mbps; its floor of 600-700 KB filled the
//! carrier's queue instead of ours (round trips of 0.3-0.9 s, and 16 s on a
//! path whose first round trip, 920 ms, was measured inside such a queue),
//! and a relay path opened mid-session got 1.8 MB at an estimate of 0.2 Mbps.
//! So the floor's rate stays under `OVER_ESTIMATE` times the best bandwidth
//! estimate the path has shown, lowered by an eighth at every loss (a still
//! screen makes BBR forget its estimate but loses nothing), and its round
//! trip under `MAX_FLOOR_RTT`.

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use noq_proto::{
    RttEstimator,
    congestion::{Bbr3Config, Controller, ControllerFactory, ControllerMetrics},
};

/// Extra time's worth of data allowed in flight above the minimum round trip.
/// Home WiFi stalls for 40-90 ms every few seconds (phase 0); the window
/// should carry the stream through such a stall.
const SLACK: Duration = Duration::from_millis(80);
/// Round trip assumed before the first ack.
const FIRST_RTT: Duration = Duration::from_millis(100);
const MAX_SEND_QUANTUM: u64 = 64 * 1024;
/// Floor for every connection, in bytes per second (2 Mbps). The viewer only
/// sends acks, reports and input, but those collapse the same way: after a
/// still screen its acks queued behind a crawling BBR, frames timed out on
/// the host and the video rate was cut to a fifth (test laptop 2026-11-06).
const MIN_RATE: u64 = 250_000;
/// The floor's rate over the path's bandwidth estimate: BBR's own probing gain.
const OVER_ESTIMATE: f64 = 1.25;
/// Longest minimum round trip the floor is sized for.
const MAX_FLOOR_RTT: Duration = Duration::from_millis(200);

/// Bytes per second the video sender currently wants to send (0 before it
/// starts). One process streams to one viewer at a time, so one value is enough.
static FLOOR: AtomicU64 = AtomicU64::new(0);
/// Pacing floor over the chosen rate, in percent; 0 turns the floor off.
static PACING_PCT: AtomicU64 = AtomicU64::new(400);

/// Pacing floor as a multiple of the chosen rate (0 = no floor at all, plain BBR).
pub fn set_pacing_gain(gain: f64) {
    PACING_PCT.store((gain.max(0.0) * 100.0) as u64, Ordering::Relaxed);
}

/// Sets the floor from the chosen video bitrate and the FEC share in percent.
pub fn set_floor(bitrate_bps: f64, fec_pct: u32) {
    let bytes = bitrate_bps / 8.0 * (1.0 + fec_pct as f64 / 100.0);
    FLOOR.store(bytes.max(0.0) as u64, Ordering::Relaxed);
}

pub fn clear_floor() {
    FLOOR.store(0, Ordering::Relaxed);
}

pub fn factory() -> Arc<Factory> {
    Arc::new(Factory { inner: Arc::new(Bbr3Config::default()) })
}

pub struct Factory {
    inner: Arc<Bbr3Config>,
}

impl ControllerFactory for Factory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Floored { inner: self.inner.clone().build(now, current_mtu), min_rtt: None, best_bits: 0 })
    }
}

#[derive(Debug)]
struct Floored {
    inner: Box<dyn Controller>,
    min_rtt: Option<Duration>,
    /// Highest bandwidth estimate (bits/s) seen, lowered at every loss.
    best_bits: u64,
}

impl Floored {
    fn estimate(&self) -> u64 {
        self.inner.metrics().bandwidth_estimate.unwrap_or(0)
    }

    /// (window bytes, pacing bytes/s) the chosen video rate needs.
    fn floor(&self) -> (u64, u64) {
        let pct = PACING_PCT.load(Ordering::Relaxed);
        if pct == 0 {
            return (0, 0);
        }
        let chosen = FLOOR.load(Ordering::Relaxed).max(MIN_RATE);
        let pacing = (chosen as u128 * pct as u128 / 100) as u64;
        let shown = (self.best_bits.max(self.estimate()) as f64 / 8.0 * OVER_ESTIMATE) as u64;
        let rate = chosen.min(shown.max(MIN_RATE));
        let t = self.min_rtt.unwrap_or(FIRST_RTT).min(MAX_FLOOR_RTT) + SLACK;
        ((rate as f64 * t.as_secs_f64()) as u64, pacing)
    }

    /// Most of BBR's own window that is kept: its bandwidth estimate for
    /// twice the minimum round trip (BBR's normal window), or for the minimum
    /// round trip plus `SLACK` (as the floor), whichever is longer. Not tied
    /// to the chosen rate: on a phone hotspot (2026-11-24) the round trip
    /// under load was 50-400 ms against a minimum of 24 ms, and a window of
    /// the chosen rate for the minimum plus `SLACK` (26 KB) let through 0.7
    /// Mbps, which looked like a slow link and cut the rate to the minimum.
    fn ceiling(&self) -> u64 {
        let Some(bits) = self.inner.metrics().bandwidth_estimate else { return u64::MAX };
        let rtt = self.min_rtt.unwrap_or(FIRST_RTT);
        let t = (rtt * 2).max(rtt + SLACK);
        (bits as f64 / 8.0 * t.as_secs_f64()) as u64
    }

    fn capped_window(&self) -> u64 {
        self.inner.window().min(self.ceiling()).max(self.floor().0)
    }
}

impl Controller for Floored {
    fn on_sent(&mut self, now: Instant, bytes: u64, largest_pn: u64) {
        self.inner.on_sent(now, bytes, largest_pn)
    }

    fn on_packet_sent(&mut self, now: Instant, bytes: u16, pn: u64) {
        self.inner.on_packet_sent(now, bytes, pn)
    }

    fn on_cwnd_limited(&mut self) {
        self.inner.on_cwnd_limited()
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, pn: u64, app_limited: bool, rtt: &RttEstimator) {
        self.min_rtt = Some(rtt.min());
        self.inner.on_ack(now, sent, bytes, pn, app_limited, rtt);
        self.best_bits = self.best_bits.max(self.estimate());
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest: Option<u64>) {
        self.inner.on_end_acks(now, in_flight, app_limited, largest)
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        persistent: bool,
        is_ecn: bool,
        lost_bytes: u64,
        largest_lost_pn: u64,
    ) {
        self.inner.on_congestion_event(now, sent, persistent, is_ecn, lost_bytes, largest_lost_pn);
        self.best_bits = self.estimate().max(self.best_bits / 8 * 7);
    }

    fn on_packet_lost(&mut self, lost_bytes: u16, pn: u64, now: Instant) {
        self.inner.on_packet_lost(lost_bytes, pn, now)
    }

    fn on_spurious_congestion_event(&mut self) {
        self.inner.on_spurious_congestion_event()
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu)
    }

    fn on_ack_frequency_update(&mut self, threshold: u64, max_ack_delay: Duration) {
        self.inner.on_ack_frequency_update(threshold, max_ack_delay)
    }

    fn window(&self) -> u64 {
        self.capped_window()
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut m = self.inner.metrics();
        let (_, pacing) = self.floor();
        m.congestion_window = self.capped_window();
        if let Some(r) = m.pacing_rate.as_mut() {
            *r = (*r).max(pacing);
        }
        // BBR sizes its bursts from its own (possibly collapsed) rate: 1 ms of it.
        if let Some(q) = m.send_quantum.as_mut() {
            *q = (*q).max((pacing / 1000).min(MAX_SEND_QUANTUM));
        }
        m
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Floored { inner: self.inner.clone_box(), min_rtt: self.min_rtt, best_bits: self.best_bits })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor is a process-wide value, so all of its checks share one test.
    #[test]
    fn floor_lifts_a_collapsed_window_and_follows_the_chosen_rate() {
        let f = factory();
        let c = f.build(Instant::now(), 1200);
        set_pacing_gain(0.0);
        let own = c.window();
        set_pacing_gain(4.0);
        set_floor(20e6, 25);
        // No estimate yet: the window floor is the minimum, 250 KB/s for
        // (100 ms first guess + 80 ms); pacing still follows the chosen rate.
        assert_eq!(c.window(), own.max(45_000));
        let m = c.metrics();
        assert_eq!(m.congestion_window, own.max(45_000));
        assert!(m.pacing_rate.is_none_or(|r| r >= 4 * 3_125_000));
        set_floor(1e6, 0);
        assert_eq!(c.window(), own.max(45_000));
        clear_floor();
        assert_eq!(c.window(), own.max(45_000));
        set_pacing_gain(0.0);
        assert_eq!(c.window(), own);
        set_pacing_gain(4.0);
        cap_follows_the_bandwidth_estimate();
    }

    /// A stand-in for a BBR whose window ran away.
    #[derive(Debug, Clone)]
    struct Runaway {
        window: u64,
        bits: u64,
    }

    impl Controller for Runaway {
        fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: bool, _: u64, _: u64) {}
        fn on_mtu_update(&mut self, _: u16) {}
        fn window(&self) -> u64 {
            self.window
        }
        fn metrics(&self) -> ControllerMetrics {
            let mut m = Arc::new(Bbr3Config::default()).build(Instant::now(), 1200).metrics();
            m.congestion_window = self.window;
            m.bandwidth_estimate = Some(self.bits);
            m
        }
        fn clone_box(&self) -> Box<dyn Controller> {
            Box::new(self.clone())
        }
        fn initial_window(&self) -> u64 {
            self.window
        }
        fn into_any(self: Box<Self>) -> Box<dyn Any> {
            self
        }
    }

    /// Runs inside the floor test: both read the process-wide floor.
    fn cap_follows_the_bandwidth_estimate() {
        let wild = |window, bits, rtt_ms| Floored {
            inner: Box::new(Runaway { window, bits }),
            min_rtt: Some(Duration::from_millis(rtt_ms)),
            best_bits: 0,
        };
        clear_floor();
        // 44 MB at a 42 Mbps estimate and 10 ms: 5.25 MB/s for 90 ms.
        let c = wild(44_000_000, 42_000_000, 10);
        assert_eq!(c.window(), 472_500);
        assert_eq!(c.metrics().congestion_window, 472_500);
        // Long paths keep BBR's own two round trips.
        assert_eq!(wild(44_000_000, 8_000_000, 200).window(), 400_000);
        // A window below the cap is BBR's own.
        assert_eq!(wild(300_000, 42_000_000, 10).window(), 300_000);
        // 20 Mbps + 25% FEC = 3.125 MB/s: a collapsed window is lifted to it
        // for 10 ms + 80 ms...
        set_floor(20e6, 25);
        assert_eq!(wild(5_000, 40_000_000, 10).window(), 281_250);
        // ...but only up to 1.25 times what the path has shown it carries: 1 MB/s.
        assert_eq!(wild(44_000_000, 8_000_000, 10).window(), 112_500);
        // An estimate forgotten on a still screen still counts; every loss
        // lowers it by an eighth, down to what BBR sees now.
        let mut c = Floored { best_bits: 40_000_000, ..wild(5_000, 1_000_000, 10) };
        assert_eq!(c.window(), 281_250);
        let now = Instant::now();
        c.on_congestion_event(now, now, false, false, 1200, 1);
        assert_eq!(c.window(), 281_250, "one loss on a still screen");
        for _ in 0..40 {
            c.on_congestion_event(now, now, false, false, 1200, 1);
        }
        assert_eq!(c.window(), 22_500, "the minimum, 250 KB/s for 90 ms");
        // A minimum round trip measured inside a standing queue counts as 200 ms.
        assert_eq!(wild(5_000, 40_000_000, 920).window(), 875_000);
        clear_floor();
    }
}
