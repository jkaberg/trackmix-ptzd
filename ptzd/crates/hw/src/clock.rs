//! Injectable time so the simulator is deterministic in tests.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub trait Clock: Send {
    /// Monotonic time since an arbitrary origin.
    fn now(&self) -> Duration;
}

/// Wall-clock mode for the mock daemon.
pub struct RealClock(Instant);
impl RealClock {
    pub fn new() -> Self {
        RealClock(Instant::now())
    }
}
impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}
impl Clock for RealClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Test clock; clones share the same time.
#[derive(Clone, Default)]
pub struct ManualClock(Arc<AtomicU64>);
impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn advance(&self, d: Duration) {
        self.0.fetch_add(d.as_nanos() as u64, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }
}
