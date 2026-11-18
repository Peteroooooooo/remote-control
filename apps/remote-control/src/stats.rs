//! Small helpers for per-second statistics.

/// Nearest-rank percentile; NaN for an empty slice.
pub fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[((p / 100.0) * (s.len() - 1) as f64).round() as usize]
}

/// Microseconds since the first call, from a monotonic clock.
pub fn now_us() -> u64 {
    use std::{sync::OnceLock, time::Instant};
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// No `log!` lines: stdout carries a remote command's output (`term`).
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
