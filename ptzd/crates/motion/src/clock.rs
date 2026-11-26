//! Time seam so tests can run on a fake clock.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary origin.
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
    /// (days since epoch, hour 0..23) in UTC, for the daily re-home.
    fn utc_day_hour(&self) -> (u64, u32);
}

pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock { start: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
    fn utc_day_hour(&self) -> (u64, u32) {
        let s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (s / 86_400, ((s % 86_400) / 3_600) as u32)
    }
}
