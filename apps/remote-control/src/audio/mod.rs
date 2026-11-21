pub mod agent;
pub mod codec;
pub mod config;
pub mod wasapi;

/// RAII guard for Windows multimedia timer resolution (`timeBeginPeriod` / `timeEndPeriod`).
pub struct TimerPeriodGuard {
    period_ms: Option<u32>,
}

impl TimerPeriodGuard {
    pub fn enter(period_ms: u32) -> Self {
        let period_ms = period_ms.max(1);
        let ok = unsafe { windows::Win32::Media::timeBeginPeriod(period_ms) } == 0;
        Self { period_ms: ok.then_some(period_ms) }
    }
}

impl Drop for TimerPeriodGuard {
    fn drop(&mut self) {
        if let Some(p) = self.period_ms.take() {
            unsafe { windows::Win32::Media::timeEndPeriod(p) };
        }
    }
}
