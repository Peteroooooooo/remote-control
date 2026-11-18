//! Wraps a QUIC congestion controller to record what it decides and, optionally,
//! allow more data in flight than the controller's own window.
//!
//! On home WiFi the round trip jumps from ~8 ms to 40-90 ms for a moment every
//! few seconds. BBR's window (about two BDPs at the minimum round trip) then
//! fills, the next frame waits in our send queue and gets skipped. A window gain
//! > 1 lets a few more frames ride through such a stall; pacing is unchanged.

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
    congestion::{Controller, ControllerFactory, ControllerMetrics},
};

/// Last values reported by the wrapped controller (of whichever path reported last).
#[derive(Debug, Default)]
pub struct CcStats {
    /// Bytes per second.
    pub pacing_rate: AtomicU64,
    /// Bytes, before the window gain.
    pub cwnd: AtomicU64,
}

pub struct Factory {
    pub inner: Arc<dyn ControllerFactory + Send + Sync>,
    pub window_gain: f64,
    pub stats: Arc<CcStats>,
}

impl ControllerFactory for Factory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Wrapped {
            inner: self.inner.clone().build(now, current_mtu),
            window_gain: self.window_gain,
            stats: self.stats.clone(),
        })
    }
}

#[derive(Debug)]
struct Wrapped {
    inner: Box<dyn Controller>,
    window_gain: f64,
    stats: Arc<CcStats>,
}

impl Controller for Wrapped {
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
        (self.inner.window() as f64 * self.window_gain) as u64
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut m = self.inner.metrics();
        self.stats.cwnd.store(m.congestion_window, Ordering::Relaxed);
        if let Some(rate) = m.pacing_rate {
            self.stats.pacing_rate.store(rate, Ordering::Relaxed);
        }
        m.congestion_window = self.window();
        m
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Wrapped { inner: self.inner.clone_box(), window_gain: self.window_gain, stats: self.stats.clone() })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}
