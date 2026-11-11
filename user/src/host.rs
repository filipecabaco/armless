//! What user mode takes from the host it runs on: a monotonic clock, the wall clock and
//! randomness. Natively these are std's (`Instant` is `std::time::Instant`) and the system's
//! random source; in WebAssembly, where std has no clock, they are armless's JavaScript imports
//! (`performance.now()`, `Date.now()`) and a generator the host seeds (`seed`).

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

/// Nanoseconds since the Unix epoch (0 if the clock is before it).
#[cfg(not(target_arch = "wasm32"))]
pub fn wall_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// `n` bytes from the system's random source.
#[cfg(not(target_arch = "wasm32"))]
pub fn random_bytes(n: usize) -> Vec<u8> {
    armless::native::random_bytes(n)
}

/// Natively there is nothing to seed: the system's random source is fresh as it is.
#[cfg(not(target_arch = "wasm32"))]
pub fn seed(_entropy: &[u8]) {}

#[cfg(target_arch = "wasm32")]
pub use wasm::{random_bytes, seed, wall_ns, Instant};

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::ops::{Add, Sub};
    use std::sync::Mutex;
    use std::time::Duration;

    use armless::dev::web::Rng;

    /// A point of the monotonic clock: nanoseconds of `performance.now()`. As much of
    /// `std::time::Instant` as user mode uses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(u64);

    impl Instant {
        pub fn now() -> Instant {
            Instant((armless::wasm::now_ms() * 1e6) as u64)
        }

        pub fn elapsed(&self) -> Duration {
            Instant::now().saturating_duration_since(*self)
        }

        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            Duration::from_nanos(self.0.saturating_sub(earlier.0))
        }

        pub fn duration_since(&self, earlier: Instant) -> Duration {
            self.saturating_duration_since(earlier)
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            Instant(
                self.0
                    .saturating_add(d.as_nanos().min(u64::MAX as u128) as u64),
            )
        }
    }

    impl Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, earlier: Instant) -> Duration {
            self.saturating_duration_since(earlier)
        }
    }

    pub fn wall_ns() -> u64 {
        (armless::wasm::wall_ms().max(0.0) * 1e6) as u64
    }

    /// The generator `random_bytes` draws from: SHA-256 in counter mode, keyed by what the host
    /// seeds it with (armless's `Rng`, which the machine's TLS uses too).
    static RNG: Mutex<Option<Rng>> = Mutex::new(None);

    /// Mix in fresh entropy from the host (crypto.getRandomValues): before a program starts and
    /// after every restore, so that copies of one snapshot don't draw the same numbers.
    pub fn seed(entropy: &[u8]) {
        let mut rng = RNG.lock().unwrap_or_else(|e| e.into_inner());
        rng.get_or_insert_with(Rng::default).seed(entropy);
    }

    pub fn random_bytes(n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        let mut rng = RNG.lock().unwrap_or_else(|e| e.into_inner());
        let rng = rng.get_or_insert_with(|| {
            // Unseeded: the clocks are better than nothing
            let mut r = Rng::default();
            r.seed(&wall_ns().to_le_bytes());
            r.seed(&Instant::now().0.to_le_bytes());
            r
        });
        rng.fill(&mut b);
        b
    }
}
