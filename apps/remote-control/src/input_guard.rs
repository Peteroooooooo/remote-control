//! Adaptive stale-input discard guard ("Input Age Guard") for remote control
//! over unstable networks.
//!
//! Viewer and host clocks are not synchronized, so the host estimates relative
//! one-way delay `obs = recv_ms - send_ms` and tracks its lower bound
//! `base_offset` (representing the best-case clock offset + physical minimum
//! propagation delay). Any excess `age_ms = obs - base_offset` above
//! `max_age_ms` represents queuing or blackout buffering delay, so stale inputs
//! are dropped cleanly without needing NTP/PTP clock sync.

pub const DEFAULT_MAX_AGE_MS: u64 = 2_000;
pub const DEFAULT_RESET_AFTER_MS: u64 = 5_000;

/// Allows `base_offset` to drift upward at up to 1 ms per 1000 ms (1000 ppm)
/// to absorb oscillator frequency skew without locking onto an ancient minimum.
const DRIFT_NUM: u64 = 1;
const DRIFT_DEN: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputVerdict {
    /// Input is fresh; inject it normally.
    Accept,
    /// Input is stale (`age_ms > max_age_ms`); discard it.
    ///
    /// `release_all` is `true` only on the first dropped event of a stale
    /// episode so the host releases held keys/buttons once instead of spamming
    /// `ReleaseAll` across an entire burst drain.
    Drop { release_all: bool },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuardStats {
    pub accepted: u64,
    pub dropped: u64,
    pub releases_triggered: u64,
    pub baseline_resets: u64,
    pub max_age_seen_ms: u64,
}

pub struct InputGuard {
    max_age_ms: u64,
    reset_after_ms: u64,

    last_send_u32: Option<u32>,
    last_send_u64: u64,

    baseline: Option<i64>,
    last_recv_ms: u64,
    drift_acc: u64,

    late_since_recv: Option<u64>,
    late_first_send: Option<u64>,
    late_min_obs: Option<i64>,
    released_in_episode: bool,

    stats: GuardStats,
}

impl Default for InputGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl InputGuard {
    pub fn new() -> Self {
        Self::with_config(DEFAULT_MAX_AGE_MS, DEFAULT_RESET_AFTER_MS)
    }

    pub fn with_config(max_age_ms: u64, reset_after_ms: u64) -> Self {
        Self {
            max_age_ms,
            reset_after_ms,
            last_send_u32: None,
            last_send_u64: 0,
            baseline: None,
            last_recv_ms: 0,
            drift_acc: 0,
            late_since_recv: None,
            late_first_send: None,
            late_min_obs: None,
            released_in_episode: false,
            stats: GuardStats::default(),
        }
    }

    #[cfg(test)]
    pub fn reset(&mut self) {
        *self = Self::with_config(self.max_age_ms, self.reset_after_ms);
    }

    #[cfg(test)]
    pub fn stats(&self) -> &GuardStats {
        &self.stats
    }

    /// Convenience helper for protocol messages where older viewers omit the
    /// sender timestamp (`None`). Always returns `Accept` without mutating
    /// baseline state when `send_ms` is `None`.
    #[cfg(test)]
    pub fn check_opt(&mut self, send_ms: Option<u32>, recv_ms: u64) -> InputVerdict {
        match send_ms {
            Some(ts) => self.check(ts, recv_ms),
            None => {
                self.stats.accepted += 1;
                InputVerdict::Accept
            }
        }
    }

    pub fn check(&mut self, send_ms_u32: u32, recv_ms: u64) -> InputVerdict {
        let send_u64 = self.unwrap_send_ms(send_ms_u32);
        let obs = recv_ms as i64 - send_u64 as i64;

        let Some(mut base_offset) = self.baseline else {
            self.baseline = Some(obs);
            self.last_recv_ms = recv_ms;
            self.stats.accepted += 1;
            return InputVerdict::Accept;
        };

        // Slow upward drift compensation for crystal skew / gradual path drift.
        let elapsed_recv = recv_ms.saturating_sub(self.last_recv_ms);
        self.drift_acc = self
            .drift_acc
            .saturating_add(elapsed_recv.saturating_mul(DRIFT_NUM));
        let step = self.drift_acc / DRIFT_DEN;
        self.drift_acc %= DRIFT_DEN;
        base_offset = base_offset.saturating_add(step as i64);
        self.last_recv_ms = self.last_recv_ms.max(recv_ms);

        // Lower-bound tracking when path improves toward physical minimum.
        if obs < base_offset {
            base_offset = obs;
            self.clear_late_episode();
        }
        self.baseline = Some(base_offset);

        let age_ms = obs.saturating_sub(base_offset).max(0) as u64;
        self.stats.max_age_seen_ms = self.stats.max_age_seen_ms.max(age_ms);

        if age_ms <= self.max_age_ms {
            self.clear_late_episode();
            self.stats.accepted += 1;
            return InputVerdict::Accept;
        }

        // Stale input: track continuous late episode by receiver time + send span
        // so a fast burst drain is never mistaken for a permanent route jump.
        let since = *self.late_since_recv.get_or_insert(recv_ms);
        let first_send = *self.late_first_send.get_or_insert(send_u64);
        let min_obs = match self.late_min_obs {
            Some(prev) => prev.min(obs),
            None => obs,
        };
        self.late_min_obs = Some(min_obs);

        let recv_span = recv_ms.saturating_sub(since);
        let send_span = send_u64.saturating_sub(first_send);
        if recv_span >= self.reset_after_ms && send_span >= self.reset_after_ms / 2 {
            let new_base = min_obs;
            self.baseline = Some(new_base);
            self.drift_acc = 0;
            self.stats.baseline_resets += 1;

            let new_age = obs.saturating_sub(new_base).max(0) as u64;
            if new_age <= self.max_age_ms {
                self.clear_late_episode();
                self.stats.accepted += 1;
                return InputVerdict::Accept;
            }

            // Still stale after resetting to the episode's minimum: begin a fresh
            // timing window without re-triggering ReleaseAll mid-episode.
            self.late_since_recv = Some(recv_ms);
            self.late_first_send = Some(send_u64);
            self.late_min_obs = Some(obs);
        }

        let release_all = !self.released_in_episode;
        self.released_in_episode = true;
        self.stats.dropped += 1;
        if release_all {
            self.stats.releases_triggered += 1;
        }
        InputVerdict::Drop { release_all }
    }

    fn unwrap_send_ms(&mut self, send_ms_u32: u32) -> u64 {
        match self.last_send_u32 {
            None => {
                let send_u64 = send_ms_u32 as u64;
                self.last_send_u32 = Some(send_ms_u32);
                self.last_send_u64 = send_u64;
                send_u64
            }
            Some(prev_u32) => {
                let delta = send_ms_u32.wrapping_sub(prev_u32) as i32 as i64;
                let send_u64 = (self.last_send_u64 as i64).saturating_add(delta).max(0) as u64;
                if delta >= 0 {
                    self.last_send_u32 = Some(send_ms_u32);
                    self.last_send_u64 = send_u64;
                }
                send_u64
            }
        }
    }

    fn clear_late_episode(&mut self) {
        self.late_since_recv = None;
        self.late_first_send = None;
        self.late_min_obs = None;
        self.released_in_episode = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_viewer_none_always_accepted_without_mutating_baseline() {
        let mut guard = InputGuard::new();

        // None before baseline initialization must not initialize baseline.
        for t in (0..10_000).step_by(500) {
            assert_eq!(guard.check_opt(None, t), InputVerdict::Accept);
        }
        assert!(guard.baseline.is_none());
        assert_eq!(guard.stats().accepted, 20);
        assert_eq!(guard.stats().dropped, 0);

        // First timestamped sample establishes baseline cleanly at its own offset.
        assert_eq!(guard.check_opt(Some(1_000), 50_000), InputVerdict::Accept);
        assert_eq!(guard.baseline, Some(49_000));

        // None after baseline initialization must not drift or shift baseline.
        assert_eq!(guard.check_opt(None, 60_000), InputVerdict::Accept);
        assert_eq!(guard.baseline, Some(49_000));
        assert_eq!(guard.last_recv_ms, 50_000);
    }

    #[test]
    fn normal_jitter_never_dropped() {
        let mut guard = InputGuard::new();
        // Unsynchronized clocks: host is +10_000 ms ahead; network jitter varies 50..800 ms.
        let jitters = [50u64, 120, 400, 800, 200, 60, 790, 50, 500, 750];
        for (i, &jitter) in jitters.iter().enumerate() {
            let send = (i as u32) * 100;
            let recv = 10_000 + send as u64 + jitter;
            assert_eq!(guard.check(send, recv), InputVerdict::Accept);
        }
        assert_eq!(guard.stats().accepted, jitters.len() as u64);
        assert_eq!(guard.stats().dropped, 0);
        assert_eq!(guard.stats().releases_triggered, 0);
        assert_eq!(guard.stats().baseline_resets, 0);
        // 1050 ms elapsed between i=0 and i=3 drifts baseline upward by 1 ms.
        assert_eq!(guard.stats().max_age_seen_ms, 749);
    }

    #[test]
    fn blackout_burst_drain_drops_all_with_single_release_then_accepts_fresh() {
        let mut guard = InputGuard::new();

        // Establish steady baseline (20 ms one-way offset).
        assert_eq!(guard.check(1_000, 1_020), InputVerdict::Accept);
        assert_eq!(guard.check(1_100, 1_120), InputVerdict::Accept);

        // 5 s blackout: viewer buffered 50 clicks between send=2000..2980,
        // all arriving in a 50 ms burst at recv=7000..7049.
        for i in 0..50u32 {
            let send = 2_000 + i * 20;
            let recv = 7_000 + i as u64;
            let verdict = guard.check(send, recv);
            assert_eq!(
                verdict,
                InputVerdict::Drop { release_all: i == 0 },
                "unexpected verdict at buffered click {i}"
            );
        }

        // Fresh input immediately after burst drain is accepted.
        assert_eq!(guard.check(7_050, 7_070), InputVerdict::Accept);

        assert_eq!(guard.stats().dropped, 50);
        assert_eq!(guard.stats().releases_triggered, 1);
        assert_eq!(guard.stats().baseline_resets, 0);
        assert_eq!(guard.stats().accepted, 3);

        // A subsequent separate stale episode triggers release_all=true once again.
        assert_eq!(
            guard.check(7_100, 11_000),
            InputVerdict::Drop { release_all: true }
        );
        assert_eq!(
            guard.check(7_120, 11_001),
            InputVerdict::Drop { release_all: false }
        );
        assert_eq!(guard.stats().releases_triggered, 2);
    }

    #[test]
    fn permanent_route_jump_recovers_after_reset_after_ms() {
        let mut guard = InputGuard::new();

        // Establish baseline at 20 ms latency.
        assert_eq!(guard.check(0, 20), InputVerdict::Accept);
        assert_eq!(guard.check(500, 520), InputVerdict::Accept);

        // Permanent +3000 ms latency jump starting at send=1000.
        // Inputs arrive every 200 ms for 4800 ms (all dropped).
        for step in 0..25u32 {
            let send = 1_000 + step * 200; // 1000..5800
            let recv = send as u64 + 3_020; // 4020..8820
            assert_eq!(
                guard.check(send, recv),
                InputVerdict::Drop {
                    release_all: step == 0
                },
                "step {step} at recv={recv} should still drop before 5000 ms"
            );
        }

        // At send=6000, recv=9020 (5000 ms since first late packet at recv=4020),
        // baseline resets to the new route minimum and this input is accepted.
        assert_eq!(guard.check(6_000, 9_020), InputVerdict::Accept);
        assert_eq!(guard.stats().baseline_resets, 1);

        // Subsequent inputs on the new route continue to be accepted.
        for step in 1..=10u32 {
            let send = 6_000 + step * 200;
            let recv = send as u64 + 3_020;
            assert_eq!(guard.check(send, recv), InputVerdict::Accept);
        }
        assert_eq!(guard.stats().dropped, 25);
        assert_eq!(guard.stats().releases_triggered, 1);
    }

    #[test]
    fn u32_wrap_around_is_seamless() {
        let mut guard = InputGuard::new();
        let start_send = u32::MAX - 500;
        let mut recv = 100_000u64;

        for offset in (0..=1_000u32).step_by(50) {
            let send = start_send.wrapping_add(offset);
            assert_eq!(guard.check(send, recv + offset as u64), InputVerdict::Accept);
        }
        recv += 1_000;
        // Verify baseline still accurately detects stale input after wrap-around.
        assert_eq!(
            guard.check(501, recv + 3_000),
            InputVerdict::Drop { release_all: true }
        );
        assert_eq!(guard.stats().dropped, 1);
    }

    #[test]
    fn clock_skew_500ppm_over_one_hour_zero_false_drops() {
        let mut guard = InputGuard::new();

        // Case 1: Host clock runs +500 ppm faster than viewer (+0.5 ms per 1000 ms).
        // Over 1 hour (3,600,000 ms), host drifts +1800 ms ahead of viewer.
        for sec in 0..=3_600u64 {
            let send = (sec * 1_000) as u32;
            let recv = 50_000 + sec * 1_000 + (sec / 2);
            assert_eq!(
                guard.check(send, recv),
                InputVerdict::Accept,
                "false drop at sec={sec} with +500ppm host skew"
            );
        }
        assert_eq!(guard.stats().dropped, 0);

        // Case 2: Viewer clock runs +500 ppm faster than host.
        guard.reset();
        for sec in 0..=3_600u64 {
            let send = (sec * 1_000 + sec / 2) as u32;
            let recv = 50_000 + sec * 1_000;
            assert_eq!(
                guard.check(send, recv),
                InputVerdict::Accept,
                "false drop at sec={sec} with +500ppm viewer skew"
            );
        }
        assert_eq!(guard.stats().dropped, 0);
    }
}
