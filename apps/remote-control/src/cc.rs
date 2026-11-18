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
        Box::new(Floored { inner: self.inner.clone().build(now, current_mtu), min_rtt: None })
    }
}

#[derive(Debug)]
struct Floored {
    inner: Box<dyn Controller>,
    min_rtt: Option<Duration>,
}

impl Floored {
    /// (window bytes, pacing bytes/s) the chosen video rate needs.
    fn floor(&self) -> (u64, u64) {
        let pct = PACING_PCT.load(Ordering::Relaxed);
        if pct == 0 {
            return (0, 0);
        }
        let rate = FLOOR.load(Ordering::Relaxed).max(MIN_RATE);
        let t = self.min_rtt.unwrap_or(FIRST_RTT) + SLACK;
        let pacing = (rate as u128 * pct as u128 / 100) as u64;
        ((rate as f64 * t.as_secs_f64()) as u64, pacing)
    }

    /// Most of BBR's own window that is kept: its bandwidth estimate for
    /// twice the minimum round trip (BBR's normal window), or for the minimum
    /// round trip plus `SLACK` (as the floor), whichever is longer.
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
        self.inner.on_ack(now, sent, bytes, pn, app_limited, rtt)
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
        self.inner.on_congestion_event(now, sent, persistent, is_ecn, lost_bytes, largest_lost_pn)
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
        Box::new(Floored { inner: self.inner.clone_box(), min_rtt: self.min_rtt })
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
        // 20 Mbps + 25% FEC = 3.125 MB/s; (100 ms first guess + 80 ms) of it.
        assert_eq!(c.window(), 562_500);
        let m = c.metrics();
        assert_eq!(m.congestion_window, 562_500);
        assert!(m.pacing_rate.is_none_or(|r| r >= 4 * 3_125_000));
        // Below 2 Mbps the minimum applies: 250 KB/s for 180 ms.
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
        // The floor still wins over a low estimate.
        set_floor(20e6, 25);
        assert_eq!(wild(44_000_000, 1_000_000, 10).window(), 281_250);
        clear_floor();
    }
}
