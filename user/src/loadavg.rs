//! What /proc/stat, /proc/loadavg and sysinfo report of the machine as a whole: the time each CPU
//! ran a thread (`busy`, counted by `Process::charge_cpu` with the CPU time of timer.rs), and the
//! load averages.
//!
//! A load average is the number of threads that run or could (not parked in a call), averaged
//! over 1, 5 and 15 minutes as Linux does: each sample moves it towards the count by `1 - e`,
//! with `e = exp(-dt / period)`. Linux samples every 5 s; here a sample is taken when a slice
//! ends at least a second after the last one, and a reading decays the averages to the moment it
//! is taken with the count then, so a machine that has been idle reads as one that is.
//!
//! Neither is saved: a restored machine counts from its restore.

use crate::host::Instant;

/// The periods of the three averages (s)
const PERIODS: [f64; 3] = [60.0, 300.0, 900.0];
/// The least time between two samples (s)
const SAMPLE: f64 = 1.0;

#[derive(Default)]
pub(crate) struct SysStats {
    /// Per CPU, the ns it ran threads
    busy: Vec<u64>,
    load: [f64; 3],
    /// When `load` was last sampled (None: never)
    at: Option<Instant>,
}

/// The averages `load`, `dt` seconds on, with `n` threads that run in that time
fn decay(load: [f64; 3], dt: f64, n: f64) -> [f64; 3] {
    let mut out = load;
    for (l, period) in out.iter_mut().zip(PERIODS) {
        let e = (-dt / period).exp();
        *l = *l * e + n * (1.0 - e);
    }
    out
}

impl SysStats {
    /// CPU `cpu` ran a thread for `ns`
    pub(crate) fn charge(&mut self, cpu: usize, ns: u64) {
        if self.busy.len() <= cpu {
            self.busy.resize(cpu + 1, 0);
        }
        self.busy[cpu] = self.busy[cpu].saturating_add(ns);
    }

    /// The ns CPU `cpu` ran threads
    pub(crate) fn busy(&self, cpu: usize) -> u64 {
        self.busy.get(cpu).copied().unwrap_or(0)
    }

    /// Takes a sample of `n` threads that run, if the last was a second or more ago
    pub(crate) fn sample(&mut self, now: Instant, n: usize) {
        match self.at {
            // (The first sample starts the clock: the averages start at 0, as at boot)
            None => self.at = Some(now),
            Some(at) => {
                let dt = now.saturating_duration_since(at).as_secs_f64();
                if dt >= SAMPLE {
                    self.load = decay(self.load, dt, n as f64);
                    self.at = Some(now);
                }
            },
        }
    }

    /// The 1, 5 and 15 minute averages at `now`, with `n` threads that run since the last sample
    pub(crate) fn load(&self, now: Instant, n: usize) -> [f64; 3] {
        match self.at {
            None => [0.0; 3],
            Some(at) => decay(self.load, now.saturating_duration_since(at).as_secs_f64(), n as f64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_minute_of_one_thread_brings_the_one_minute_average_to_1_minus_1_over_e() {
        let l = decay([0.0; 3], 60.0, 1.0);
        assert!((l[0] - (1.0 - (-1.0f64).exp())).abs() < 1e-9, "{l:?}");
        // The longer averages move less
        assert!(l[0] > l[1] && l[1] > l[2] && l[2] > 0.0, "{l:?}");
    }

    #[test]
    fn steady_load_stays_and_idle_decays() {
        let l = decay([2.0; 3], 30.0, 2.0);
        assert!(l.iter().all(|x| (x - 2.0).abs() < 1e-9), "{l:?}");
        let l = decay([2.0; 3], 600.0, 0.0);
        assert!(l[0] < 0.01 && l[2] > 1.0, "{l:?}");
    }

    #[test]
    fn samples_are_a_second_apart_at_least_and_a_reading_decays_to_now() {
        let mut s = SysStats::default();
        let t0 = Instant::now();
        assert_eq!(s.load(t0, 3), [0.0; 3]);
        s.sample(t0, 1);
        assert_eq!(s.load(t0, 1), [0.0; 3]);
        // (Too soon: no sample)
        s.sample(t0 + Duration::from_millis(500), 1);
        assert_eq!(s.at, Some(t0));
        s.sample(t0 + Duration::from_secs(60), 1);
        assert!((s.load[0] - (1.0 - (-1.0f64).exp())).abs() < 1e-9);
        // A reading later, with nothing running, is lower than the last sample
        let later = s.load(t0 + Duration::from_secs(120), 0);
        assert!(later[0] < s.load[0]);
    }

    #[test]
    fn busy_is_counted_per_cpu() {
        let mut s = SysStats::default();
        s.charge(2, 5);
        s.charge(2, 7);
        s.charge(0, 1);
        assert_eq!((s.busy(0), s.busy(1), s.busy(2), s.busy(9)), (1, 0, 12, 0));
    }
}
