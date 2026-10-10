//! The native platform: host time (shared by every CPU of a machine, and
//! fast-forwardable for tests) and threads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::board::Platform;

/// A machine's clock: monotonic time since it was created, plus a skew that
/// lets a host skip ahead while the guest idles.
pub struct Clock {
    start: Instant,
    skew_ns: AtomicU64,
}

impl Clock {
    pub fn new() -> Arc<Clock> {
        Arc::new(Clock {
            start: Instant::now(),
            skew_ns: AtomicU64::new(0),
        })
    }

    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64 + self.skew_ns.load(Ordering::Relaxed)
    }

    /// Jump ahead to `ns` (if it is in the future).
    pub fn advance_to(&self, ns: u64) {
        let now = self.now_ns();
        if ns > now {
            self.skew_ns.fetch_add(ns - now, Ordering::Relaxed);
        }
    }
}

pub fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct NativePlatform {
    pub clock: Arc<Clock>,
}

impl NativePlatform {
    pub fn new(clock: Arc<Clock>) -> NativePlatform {
        NativePlatform { clock }
    }
}

impl Platform for NativePlatform {
    fn now_ns(&mut self) -> u64 {
        self.clock.now_ns()
    }
    fn wall_clock_secs(&mut self) -> u64 {
        unix_secs()
    }
    fn yield_now(&mut self) {
        std::thread::yield_now();
    }
}
