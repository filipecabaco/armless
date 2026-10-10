//! GICv2 (distributor + one CPU interface per CPU), enough for Linux's
//! `arm,cortex-a15-gic` driver, for one to eight CPUs.
//!
//! Interrupts 0..31 (SGIs and PPIs) are banked per CPU; SPIs (32..) are
//! shared and go to the CPUs in their ITARGETSR byte. Interrupt inputs are
//! level lines driven by devices (`set_line`); software can additionally
//! set/clear pending state. Edge-configured interrupts latch pending on a
//! rising edge.
//!
//! Calls that change state return a mask of CPUs whose interrupt output may
//! have changed, so the caller can kick them.

use crate::smp::MAX_CPUS;

pub const NUM_IRQS: usize = 128;
const NUM_BANKED: usize = 32;
const NUM_SHARED: usize = NUM_IRQS - NUM_BANKED;
const SPURIOUS: u32 = 1023;
const IDLE_PRIORITY: u32 = 0x100;

#[derive(Clone, Copy, Default)]
struct Irq {
    enabled: bool,
    /// Software / edge-latched pending bit.
    latched: bool,
    /// Current input line level from the device.
    line: bool,
    active: bool,
    edge: bool,
    group: bool,
    priority: u8,
}

impl Irq {
    fn pending(&self) -> bool {
        self.latched || (!self.edge && self.line)
    }

    fn flags(&self) -> u8 {
        (self.enabled as u8)
            | (self.latched as u8) << 1
            | (self.line as u8) << 2
            | (self.active as u8) << 3
            | (self.edge as u8) << 4
            | (self.group as u8) << 5
    }

    fn set_flags(&mut self, f: u8) {
        self.enabled = f & 1 != 0;
        self.latched = f & 2 != 0;
        self.line = f & 4 != 0;
        self.active = f & 8 != 0;
        self.edge = f & 16 != 0;
        self.group = f & 32 != 0;
    }
}

#[derive(Clone)]
struct CpuIf {
    enabled: bool,
    pmr: u32,
    bpr: u32,
    banked: [Irq; NUM_BANKED],
    /// Stack of active interrupt ids, most recent last (for running priority).
    active_stack: Vec<u32>,
}

impl CpuIf {
    fn new() -> CpuIf {
        let mut banked = [Irq::default(); NUM_BANKED];
        // SGIs are edge-triggered by definition.
        for irq in banked.iter_mut().take(16) {
            irq.edge = true;
        }
        CpuIf {
            enabled: false,
            pmr: 0,
            bpr: 0,
            banked,
            active_stack: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Gic {
    ncpus: usize,
    pub dist_enabled: bool,
    cpu: Vec<CpuIf>,
    shared: [Irq; NUM_SHARED],
    /// ITARGETSR of each SPI (a CPU bit mask).
    targets: [u8; NUM_SHARED],
}

impl Gic {
    pub fn new(ncpus: usize) -> Gic {
        assert!((1..=MAX_CPUS).contains(&ncpus));
        Gic {
            ncpus,
            dist_enabled: false,
            cpu: (0..ncpus).map(|_| CpuIf::new()).collect(),
            shared: [Irq::default(); NUM_SHARED],
            // SPIs go to CPU 0 until software says otherwise.
            targets: [1; NUM_SHARED],
        }
    }

    pub fn ncpus(&self) -> usize {
        self.ncpus
    }

    fn all(&self) -> u32 {
        (1u32 << self.ncpus) - 1
    }

    fn irq(&self, cpu: usize, i: usize) -> &Irq {
        if i < NUM_BANKED {
            &self.cpu[cpu].banked[i]
        } else {
            &self.shared[i - NUM_BANKED]
        }
    }

    fn irq_mut(&mut self, cpu: usize, i: usize) -> &mut Irq {
        if i < NUM_BANKED {
            &mut self.cpu[cpu].banked[i]
        } else {
            &mut self.shared[i - NUM_BANKED]
        }
    }

    /// CPUs that see interrupt `i` (as changed from `cpu`'s point of view).
    fn audience(&self, cpu: usize, i: usize) -> u32 {
        if i < NUM_BANKED {
            1 << cpu
        } else if self.ncpus == 1 {
            1
        } else {
            self.targets[i - NUM_BANKED] as u32 & self.all()
        }
    }

    /// Drive a device input line. Banked lines (PPIs) belong to `cpu`.
    pub fn set_line(&mut self, cpu: usize, irq: u32, level: bool) -> u32 {
        let i = irq as usize;
        if i >= NUM_IRQS {
            return 0;
        }
        let s = self.irq_mut(cpu, i);
        if s.line == level {
            return 0;
        }
        if s.edge && level {
            s.latched = true;
        }
        s.line = level;
        self.audience(cpu, i)
    }

    /// Current level of a shared device input line.
    pub fn line_level(&self, irq: u32) -> bool {
        let i = irq as usize;
        (NUM_BANKED..NUM_IRQS).contains(&i) && self.shared[i - NUM_BANKED].line
    }

    fn running_priority(&self, cpu: usize) -> u32 {
        self.cpu[cpu]
            .active_stack
            .iter()
            .map(|&i| self.irq(cpu, i as usize).priority as u32)
            .min()
            .unwrap_or(IDLE_PRIORITY)
    }

    /// Highest-priority pending interrupt that could be signalled to `cpu`.
    fn highest_pending(&self, cpu: usize) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32)> = None;
        for i in 0..NUM_IRQS {
            let s = self.irq(cpu, i);
            if s.enabled
                && s.pending()
                && !s.active
                && (i < NUM_BANKED || self.audience(cpu, i) & (1 << cpu) != 0)
            {
                let p = s.priority as u32;
                if best.is_none_or(|(_, bp)| p < bp) {
                    best = Some((i as u32, p));
                }
            }
        }
        best
    }

    /// Whether `cpu`'s IRQ input is asserted.
    pub fn irq_asserted(&self, cpu: usize) -> bool {
        if !self.dist_enabled || !self.cpu[cpu].enabled {
            return false;
        }
        match self.highest_pending(cpu) {
            Some((_, p)) => p < self.cpu[cpu].pmr && p < self.running_priority(cpu),
            None => false,
        }
    }

    fn acknowledge(&mut self, cpu: usize) -> (u32, u32) {
        if !self.dist_enabled || !self.cpu[cpu].enabled {
            return (SPURIOUS, 0);
        }
        match self.highest_pending(cpu) {
            Some((i, p)) if p < self.cpu[cpu].pmr && p < self.running_priority(cpu) => {
                let u = i as usize;
                let s = self.irq_mut(cpu, u);
                s.active = true;
                // Acknowledging consumes the latched pending state; a level
                // input that is still high stays pending through `line`.
                s.latched = false;
                self.cpu[cpu].active_stack.push(i);
                // Other targets of an SPI must stop seeing it.
                (i, self.audience(cpu, u) & !(1 << cpu))
            },
            _ => (SPURIOUS, 0),
        }
    }

    fn end_of_interrupt(&mut self, cpu: usize, irq: u32) -> u32 {
        let i = (irq & 0x3ff) as usize;
        if i >= NUM_IRQS {
            return 0;
        }
        self.irq_mut(cpu, i).active = false;
        let stack = &mut self.cpu[cpu].active_stack;
        if let Some(pos) = stack.iter().rposition(|&a| a as usize == i) {
            stack.remove(pos);
        }
        self.audience(cpu, i)
    }

    fn bitmap_read(&self, cpu: usize, word: usize, get: impl Fn(&Irq) -> bool) -> u32 {
        let mut v = 0;
        for b in 0..32 {
            let i = word * 32 + b;
            if i < NUM_IRQS && get(self.irq(cpu, i)) {
                v |= 1 << b;
            }
        }
        v
    }

    /// Apply `f` to every interrupt whose bit is set; returns affected CPUs.
    fn bitmap_apply(&mut self, cpu: usize, word: usize, value: u32, f: impl Fn(&mut Irq)) -> u32 {
        let mut mask = 0;
        for b in 0..32 {
            let i = word * 32 + b;
            if i < NUM_IRQS && value & (1 << b) != 0 {
                f(self.irq_mut(cpu, i));
                mask |= self.audience(cpu, i);
            }
        }
        mask
    }

    pub fn dist_read(&mut self, cpu: usize, off: u64, size: u32) -> u64 {
        let off = off as usize;
        // Byte-addressed registers
        if (0x400..0x400 + NUM_IRQS).contains(&off) {
            let mut v = 0u64;
            for k in 0..size as usize {
                if off + k < 0x400 + NUM_IRQS {
                    v |= (self.irq(cpu, off - 0x400 + k).priority as u64) << (8 * k);
                }
            }
            return v;
        }
        if (0x800..0x800 + NUM_IRQS).contains(&off) {
            // ITARGETSR: banked ones read as the reading CPU's own bit.
            let mut v = 0u64;
            for k in 0..size as usize {
                let i = off - 0x800 + k;
                let t = if self.ncpus == 1 {
                    if i < NUM_IRQS {
                        1
                    } else {
                        0
                    }
                } else if i < NUM_BANKED {
                    1 << cpu
                } else if i < NUM_IRQS {
                    self.targets[i - NUM_BANKED] as u64
                } else {
                    0
                };
                v |= t << (8 * k);
            }
            return v;
        }
        let word = (off & 0x7f) >> 2;
        let v: u32 = match off & !3 {
            0x000 => self.dist_enabled as u32,
            0x004 => (((NUM_IRQS as u32 / 32) - 1) & 0x1f) | ((self.ncpus as u32 - 1) << 5),
            0x008 => 0x0200_043B,
            0x080..=0x0fc => self.bitmap_read(cpu, word, |s| s.group),
            0x100..=0x17c | 0x180..=0x1fc => self.bitmap_read(cpu, word, |s| s.enabled),
            0x200..=0x27c | 0x280..=0x2fc => self.bitmap_read(cpu, word, |s| s.pending()),
            0x300..=0x37c | 0x380..=0x3fc => self.bitmap_read(cpu, word, |s| s.active),
            0xc00..=0xcfc => {
                let base = ((off - 0xc00) >> 2) * 16;
                let mut v = 0;
                for k in 0..16 {
                    let i = base + k;
                    if i < NUM_IRQS && self.irq(cpu, i).edge {
                        v |= 2 << (2 * k);
                    }
                }
                v
            },
            0xfe8 => 0x20, // PIDR2: GICv2
            _ => 0,
        };
        if off & 3 != 0 {
            (v >> (8 * (off & 3))) as u64
        } else {
            v as u64
        }
    }

    /// Returns the CPUs whose interrupt state may have changed.
    pub fn dist_write(&mut self, cpu: usize, off: u64, size: u32, value: u64) -> u32 {
        let off = off as usize;
        if (0x400..0x400 + NUM_IRQS).contains(&off) {
            let mut mask = 0;
            for k in 0..size as usize {
                let i = off - 0x400 + k;
                if i < NUM_IRQS {
                    self.irq_mut(cpu, i).priority = (value >> (8 * k)) as u8;
                    mask |= self.audience(cpu, i);
                }
            }
            return mask;
        }
        if (0x800..0xc00).contains(&off) {
            // ITARGETSR: only the SPI bytes are writable.
            if self.ncpus > 1 {
                for k in 0..size as usize {
                    let i = off - 0x800 + k;
                    if (NUM_BANKED..NUM_IRQS).contains(&i) {
                        self.targets[i - NUM_BANKED] = (value >> (8 * k)) as u8;
                    }
                }
                return self.all();
            }
            return 0;
        }
        let v = value as u32;
        let word = (off & 0x7f) >> 2;
        match off & !3 {
            0x000 => {
                self.dist_enabled = v & 1 != 0;
                self.all()
            },
            0x080..=0x0fc => {
                let mut mask = 0;
                for b in 0..32 {
                    let i = word * 32 + b;
                    if i < NUM_IRQS {
                        self.irq_mut(cpu, i).group = v & (1 << b) != 0;
                        mask |= self.audience(cpu, i);
                    }
                }
                mask
            },
            0x100..=0x17c => self.bitmap_apply(cpu, word, v, |s| s.enabled = true),
            0x180..=0x1fc => self.bitmap_apply(cpu, word, v, |s| s.enabled = false),
            0x200..=0x27c => self.bitmap_apply(cpu, word, v, |s| s.latched = true),
            0x280..=0x2fc => self.bitmap_apply(cpu, word, v, |s| s.latched = false),
            0x300..=0x37c => self.bitmap_apply(cpu, word, v, |s| s.active = true),
            0x380..=0x3fc => self.bitmap_apply(cpu, word, v, |s| s.active = false),
            0xc00..=0xcfc => {
                let base = ((off - 0xc00) >> 2) * 16;
                for k in 0..16 {
                    let i = base + k;
                    // SGI configuration is read-only.
                    if (16..NUM_IRQS).contains(&i) {
                        self.irq_mut(cpu, i).edge = (v >> (2 * k)) & 2 != 0;
                    }
                }
                0
            },
            0xf00 => {
                // GICD_SGIR
                let id = (v & 0xf) as usize;
                let targets = match (v >> 24) & 3 {
                    0 => (v >> 16) & 0xff & self.all(),
                    1 => self.all() & !(1 << cpu),
                    2 => 1 << cpu,
                    _ => 0,
                };
                for t in 0..self.ncpus {
                    if targets & (1 << t) != 0 {
                        self.cpu[t].banked[id].latched = true;
                    }
                }
                targets
            },
            _ => 0,
        }
    }

    /// GICC_IAR reads acknowledge; returns (value, CPUs to kick).
    pub fn cpu_read(&mut self, cpu: usize, off: u64) -> (u64, u32) {
        let c = &self.cpu[cpu];
        let (v, kick) = match off {
            0x00 => (c.enabled as u32, 0),
            0x04 => (c.pmr, 0),
            0x08 => (c.bpr, 0),
            0x0c => self.acknowledge(cpu),
            0x14 => (self.running_priority(cpu).min(0xff), 0),
            0x18 => (self.highest_pending(cpu).map_or(SPURIOUS, |(i, _)| i), 0),
            0xfc => (0x0202_043B, 0),
            _ => (0, 0),
        };
        (v as u64, kick)
    }

    /// Returns the CPUs whose interrupt state may have changed.
    pub fn cpu_write(&mut self, cpu: usize, off: u64, value: u64) -> u32 {
        let v = value as u32;
        let c = &mut self.cpu[cpu];
        match off {
            0x00 => c.enabled = v & 1 != 0,
            0x04 => c.pmr = v & 0xff,
            0x08 => c.bpr = v & 7,
            0x10 => return self.end_of_interrupt(cpu, v),
            0x1000 => {}, // GICC_DIR: only used with EOImode=1
            _ => {},
        }
        1 << cpu
    }

    /// Serialise for save_state.
    pub fn save(&self, out: &mut Vec<u8>) {
        out.push(self.dist_enabled as u8);
        out.push(self.ncpus as u8);
        for c in &self.cpu {
            out.push(c.enabled as u8);
            out.extend_from_slice(&c.pmr.to_le_bytes());
            out.extend_from_slice(&c.bpr.to_le_bytes());
            for irq in &c.banked {
                out.push(irq.flags());
                out.push(irq.priority);
            }
            out.extend_from_slice(&(c.active_stack.len() as u32).to_le_bytes());
            for &a in &c.active_stack {
                out.extend_from_slice(&a.to_le_bytes());
            }
        }
        for (irq, &t) in self.shared.iter().zip(&self.targets) {
            out.push(irq.flags());
            out.push(irq.priority);
            out.push(t);
        }
    }

    pub fn load(&mut self, inp: &mut &[u8]) -> Option<()> {
        let mut take = |n: usize| -> Option<Vec<u8>> {
            if inp.len() < n {
                return None;
            }
            let (a, b) = inp.split_at(n);
            *inp = b;
            Some(a.to_vec())
        };
        let h = take(2)?;
        if h[1] as usize != self.ncpus {
            return None; // snapshot from a machine with another CPU count
        }
        self.dist_enabled = h[0] != 0;
        for c in self.cpu.iter_mut() {
            let b = take(9)?;
            c.enabled = b[0] != 0;
            c.pmr = u32::from_le_bytes(b[1..5].try_into().ok()?);
            c.bpr = u32::from_le_bytes(b[5..9].try_into().ok()?);
            for irq in c.banked.iter_mut() {
                let b = take(2)?;
                irq.set_flags(b[0]);
                irq.priority = b[1];
            }
            let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            c.active_stack.clear();
            for _ in 0..n {
                c.active_stack
                    .push(u32::from_le_bytes(take(4)?.try_into().ok()?));
            }
        }
        for k in 0..NUM_SHARED {
            let b = take(3)?;
            self.shared[k].set_flags(b[0]);
            self.shared[k].priority = b[1];
            self.targets[k] = b[2];
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_irq_flow() {
        let mut g = Gic::new(1);
        g.dist_write(0, 0x000, 4, 1);
        g.cpu_write(0, 0x00, 1);
        g.cpu_write(0, 0x04, 0xf0);
        g.dist_write(0, 0x100 + 4, 4, 1 << (40 - 32)); // enable SPI 40
        assert!(!g.irq_asserted(0));
        g.set_line(0, 40, true);
        assert!(g.irq_asserted(0));
        assert_eq!(g.cpu_read(0, 0x0c).0, 40);
        assert!(!g.irq_asserted(0), "active interrupt must not re-signal");
        g.set_line(0, 40, false);
        g.cpu_write(0, 0x10, 40);
        assert!(!g.irq_asserted(0));
        assert_eq!(g.cpu_read(0, 0x0c).0, SPURIOUS as u64);
    }

    #[test]
    fn priority_mask_blocks() {
        let mut g = Gic::new(1);
        g.dist_write(0, 0x000, 4, 1);
        g.cpu_write(0, 0x00, 1);
        g.cpu_write(0, 0x04, 0x80);
        g.dist_write(0, 0x400 + 27, 1, 0xa0);
        g.dist_write(0, 0x100, 4, 1 << 27);
        g.set_line(0, 27, true);
        assert!(!g.irq_asserted(0));
        g.cpu_write(0, 0x04, 0xf0);
        assert!(g.irq_asserted(0));
    }

    #[test]
    fn sgi_and_targets() {
        let mut g = Gic::new(4);
        g.dist_write(0, 0x000, 4, 1);
        for c in 0..4 {
            g.cpu_write(c, 0x00, 1);
            g.cpu_write(c, 0x04, 0xf0);
            g.dist_write(c, 0x100, 4, 0xffff); // enable SGIs (banked)
        }
        // ITARGETSR0 reads back the reader's own bit
        assert_eq!(g.dist_read(2, 0x800, 4), 0x0404_0404);
        // SGI 1 to CPUs 1 and 3
        assert_eq!(g.dist_write(0, 0xf00, 4, (0b1010 << 16) | 1), 0b1010);
        assert!(!g.irq_asserted(0) && g.irq_asserted(1) && !g.irq_asserted(2) && g.irq_asserted(3));
        assert_eq!(g.cpu_read(1, 0x0c).0, 1);
        assert!(!g.irq_asserted(1) && g.irq_asserted(3));
        // SPI 40 targeted at CPU 2
        g.dist_write(0, 0x800 + 40, 1, 0b100);
        g.dist_write(0, 0x104, 4, 1 << 8);
        assert_eq!(g.set_line(0, 40, true), 0b100);
        assert!(g.irq_asserted(2));
        assert_eq!(g.cpu_read(2, 0x0c).0, 40);
    }
}
