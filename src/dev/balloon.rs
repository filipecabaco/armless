//! virtio-balloon, for one thing only: free page reporting (VIRTIO_BALLOON_F_REPORTING). The
//! guest's kernel tells the device which of its pages are free, in large blocks (Linux reports
//! free blocks of pageblock order, 2 MiB with 4 KiB pages, a couple of seconds after they free
//! up), and the device zeroes them, giving the host's memory back where it can (`Ram::discard`).
//! A snapshot leaves zero pages out, so it no longer carries memory the guest freed: what a
//! program allocated, wrote and gave back (and whatever secrets it held).
//!
//! The balloon itself never inflates: `num_pages` stays 0, so the inflate and deflate queues only
//! ever see the driver's own housekeeping, which is answered and ignored.
//!
//! What the guest reports is its business: a range is zeroed only where it is all RAM, and code
//! compiled or decoded from those pages is dropped on every CPU (`Io::discard`), as for a DMA
//! write. A guest that reports a page it still uses finds it zero, which is its own doing.

use super::virtio::Transport;
use super::Io;
use crate::ser::{R, W};

/// VIRTIO_BALLOON_F_REPORTING
const F_REPORTING: u64 = 1 << 5;

/// Queues: inflate, deflate, and (with REPORTING, and no statistics or hinting queues before it)
/// the reporting queue.
const REPORTING: usize = 2;

#[derive(Default)]
pub struct Balloon {
    /// Bytes the guest reported free and the device zeroed, since the machine started
    pub reported: u64,
}

impl Balloon {
    pub const FEATURES: u64 = F_REPORTING;

    /// `num_pages` (0: never inflate) and `actual`, little-endian.
    pub fn config_read(&self, off: u64) -> u8 {
        let _ = off;
        0
    }

    pub fn reset(&mut self) {}

    pub fn notify(&mut self, io: &Io, t: &mut Transport, q: usize) {
        let mut done = false;
        while let Some(chain) = t.queues[q].pop(io) {
            if q == REPORTING {
                // Each buffer is a run of free guest memory, handed over device-writable
                for &(addr, len) in &chain.writable {
                    if io.discard(addr, len as u64) {
                        self.reported += len as u64;
                    }
                }
            }
            if t.queues[q].push(io, chain.head, 0) {
                done = true;
            }
        }
        if done {
            t.interrupt(io);
        }
    }

    pub fn save(&self, w: &mut W) {
        w.u64(self.reported);
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.reported = r.u64()?;
        Some(())
    }
}
