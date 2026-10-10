//! A single-CPU GICv2 (distributor + CPU interface), enough for Linux's
//! `arm,cortex-a15-gic` driver.
//!
//! Interrupt inputs are level lines driven by devices (`set_line`); software
//! can additionally set/clear pending state. Edge-configured interrupts latch
//! pending on a rising edge.

pub const NUM_IRQS: usize = 128;
const SPURIOUS: u32 = 1023;
const IDLE_PRIORITY: u32 = 0x100;

#[derive(Clone)]
pub struct Gic {
    pub dist_enabled: bool,
    pub cpu_enabled: bool,
    pub pmr: u32,
    pub bpr: u32,
    enabled: [bool; NUM_IRQS],
    /// Software / edge-latched pending bit.
    latched: [bool; NUM_IRQS],
    /// Current input line level from the device.
    line: [bool; NUM_IRQS],
    active: [bool; NUM_IRQS],
    edge: [bool; NUM_IRQS],
    group: [bool; NUM_IRQS],
    priority: [u8; NUM_IRQS],
    /// Stack of active interrupt ids, most recent last (for running priority).
    active_stack: Vec<u32>,
}

impl Default for Gic {
    fn default() -> Self {
        Self::new()
    }
}

impl Gic {
    pub fn new() -> Gic {
        let mut edge = [false; NUM_IRQS];
        // SGIs are edge-triggered by definition.
        for e in edge.iter_mut().take(16) {
            *e = true;
        }
        Gic {
            dist_enabled: false,
            cpu_enabled: false,
            pmr: 0,
            bpr: 0,
            enabled: [false; NUM_IRQS],
            latched: [false; NUM_IRQS],
            line: [false; NUM_IRQS],
            active: [false; NUM_IRQS],
            edge,
            group: [false; NUM_IRQS],
            priority: [0; NUM_IRQS],
            active_stack: Vec::new(),
        }
    }

    pub fn set_line(&mut self, irq: u32, level: bool) {
        let i = irq as usize;
        if i >= NUM_IRQS {
            return;
        }
        if self.edge[i] && level && !self.line[i] {
            self.latched[i] = true;
        }
        self.line[i] = level;
    }

    /// Current level of a device input line.
    pub fn line_level(&self, irq: u32) -> bool {
        (irq as usize) < NUM_IRQS && self.line[irq as usize]
    }

    fn pending(&self, i: usize) -> bool {
        self.latched[i] || (!self.edge[i] && self.line[i])
    }

    fn running_priority(&self) -> u32 {
        self.active_stack
            .iter()
            .map(|&i| self.priority[i as usize] as u32)
            .min()
            .unwrap_or(IDLE_PRIORITY)
    }

    /// Highest-priority pending interrupt that could be signalled, if any.
    fn highest_pending(&self) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32)> = None;
        for i in 0..NUM_IRQS {
            if self.enabled[i] && self.pending(i) && !self.active[i] {
                let p = self.priority[i] as u32;
                if best.is_none_or(|(_, bp)| p < bp) {
                    best = Some((i as u32, p));
                }
            }
        }
        best
    }

    /// Whether the CPU's IRQ input is asserted.
    pub fn irq_asserted(&self) -> bool {
        if !self.dist_enabled || !self.cpu_enabled {
            return false;
        }
        match self.highest_pending() {
            Some((_, p)) => p < self.pmr && p < self.running_priority(),
            None => false,
        }
    }

    fn acknowledge(&mut self) -> u32 {
        if !self.dist_enabled || !self.cpu_enabled {
            return SPURIOUS;
        }
        match self.highest_pending() {
            Some((i, p)) if p < self.pmr && p < self.running_priority() => {
                let u = i as usize;
                self.active[u] = true;
                // Acknowledging consumes the latched pending state; a level
                // input that is still high stays pending through `line`.
                self.latched[u] = false;
                self.active_stack.push(i);
                i
            },
            _ => SPURIOUS,
        }
    }

    fn end_of_interrupt(&mut self, irq: u32) {
        let i = (irq & 0x3ff) as usize;
        if i >= NUM_IRQS {
            return;
        }
        self.active[i] = false;
        if let Some(pos) = self.active_stack.iter().rposition(|&a| a as usize == i) {
            self.active_stack.remove(pos);
        }
    }

    fn bitmap_read(arr: &[bool; NUM_IRQS], word: usize) -> u32 {
        let mut v = 0;
        for b in 0..32 {
            let i = word * 32 + b;
            if i < NUM_IRQS && arr[i] {
                v |= 1 << b;
            }
        }
        v
    }

    fn bitmap_apply(arr: &mut [bool; NUM_IRQS], word: usize, value: u32, set: bool) {
        for b in 0..32 {
            let i = word * 32 + b;
            if i < NUM_IRQS && value & (1 << b) != 0 {
                arr[i] = set;
            }
        }
    }

    pub fn dist_read(&mut self, off: u64, size: u32) -> u64 {
        let off = off as usize;
        // Byte-addressed registers
        if (0x400..0x400 + NUM_IRQS).contains(&off) {
            let mut v = 0u64;
            for k in 0..size as usize {
                if off + k < 0x400 + NUM_IRQS {
                    v |= (self.priority[off - 0x400 + k] as u64) << (8 * k);
                }
            }
            return v;
        }
        if (0x800..0x800 + NUM_IRQS).contains(&off) {
            // Single CPU: everything targets CPU 0.
            let mut v = 0u64;
            for k in 0..size as usize {
                v |= 1u64 << (8 * k);
            }
            return v;
        }
        let word = (off & 0x7f) >> 2;
        let v: u32 = match off & !3 {
            0x000 => self.dist_enabled as u32,
            0x004 => ((NUM_IRQS as u32 / 32) - 1) & 0x1f,
            0x008 => 0x0200_043B,
            0x080..=0x0fc => Self::bitmap_read(&self.group, word),
            0x100..=0x17c | 0x180..=0x1fc => Self::bitmap_read(&self.enabled, word),
            0x200..=0x27c | 0x280..=0x2fc => {
                let mut v = 0;
                for b in 0..32 {
                    let i = word * 32 + b;
                    if i < NUM_IRQS && self.pending(i) {
                        v |= 1 << b;
                    }
                }
                v
            },
            0x300..=0x37c | 0x380..=0x3fc => Self::bitmap_read(&self.active, word),
            0xc00..=0xcfc => {
                let base = ((off - 0xc00) >> 2) * 16;
                let mut v = 0;
                for k in 0..16 {
                    let i = base + k;
                    if i < NUM_IRQS && self.edge[i] {
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

    pub fn dist_write(&mut self, off: u64, size: u32, value: u64) {
        let off = off as usize;
        if (0x400..0x400 + NUM_IRQS).contains(&off) {
            for k in 0..size as usize {
                if off + k < 0x400 + NUM_IRQS {
                    self.priority[off - 0x400 + k] = (value >> (8 * k)) as u8;
                }
            }
            return;
        }
        if (0x800..0xc00).contains(&off) {
            return; // ITARGETSR: single CPU
        }
        let v = value as u32;
        let word = (off & 0x7f) >> 2;
        match off & !3 {
            0x000 => self.dist_enabled = v & 1 != 0,
            0x080..=0x0fc => {
                for b in 0..32 {
                    let i = word * 32 + b;
                    if i < NUM_IRQS {
                        self.group[i] = v & (1 << b) != 0;
                    }
                }
            },
            0x100..=0x17c => Self::bitmap_apply(&mut self.enabled, word, v, true),
            0x180..=0x1fc => Self::bitmap_apply(&mut self.enabled, word, v, false),
            0x200..=0x27c => Self::bitmap_apply(&mut self.latched, word, v, true),
            0x280..=0x2fc => Self::bitmap_apply(&mut self.latched, word, v, false),
            0x300..=0x37c => Self::bitmap_apply(&mut self.active, word, v, true),
            0x380..=0x3fc => Self::bitmap_apply(&mut self.active, word, v, false),
            0xc00..=0xcfc => {
                let base = ((off - 0xc00) >> 2) * 16;
                for k in 0..16 {
                    let i = base + k;
                    // SGI configuration is read-only.
                    if (16..NUM_IRQS).contains(&i) {
                        self.edge[i] = (v >> (2 * k)) & 2 != 0;
                    }
                }
            },
            0xf00 => {
                // GICD_SGIR: single CPU, so any target list hits us.
                let id = (v & 0xf) as usize;
                let filter = (v >> 24) & 3;
                let targets = (v >> 16) & 0xff;
                if filter == 2 || (filter == 0 && targets & 1 != 0) {
                    self.latched[id] = true;
                }
            },
            _ => {},
        }
    }

    pub fn cpu_read(&mut self, off: u64) -> u64 {
        (match off {
            0x00 => self.cpu_enabled as u32,
            0x04 => self.pmr,
            0x08 => self.bpr,
            0x0c => self.acknowledge(),
            0x14 => self.running_priority().min(0xff),
            0x18 => self.highest_pending().map_or(SPURIOUS, |(i, _)| i),
            0xfc => 0x0202_043B,
            _ => 0,
        }) as u64
    }

    pub fn cpu_write(&mut self, off: u64, value: u64) {
        let v = value as u32;
        match off {
            0x00 => self.cpu_enabled = v & 1 != 0,
            0x04 => self.pmr = v & 0xff,
            0x08 => self.bpr = v & 7,
            0x10 => self.end_of_interrupt(v),
            0x1000 => {}, // GICC_DIR: only used with EOImode=1
            _ => {},
        }
    }

    /// Serialise for save_state.
    pub fn save(&self, out: &mut Vec<u8>) {
        out.push(self.dist_enabled as u8);
        out.push(self.cpu_enabled as u8);
        out.extend_from_slice(&self.pmr.to_le_bytes());
        out.extend_from_slice(&self.bpr.to_le_bytes());
        for i in 0..NUM_IRQS {
            let flags = (self.enabled[i] as u8)
                | (self.latched[i] as u8) << 1
                | (self.line[i] as u8) << 2
                | (self.active[i] as u8) << 3
                | (self.edge[i] as u8) << 4
                | (self.group[i] as u8) << 5;
            out.push(flags);
            out.push(self.priority[i]);
        }
        out.extend_from_slice(&(self.active_stack.len() as u32).to_le_bytes());
        for &a in &self.active_stack {
            out.extend_from_slice(&a.to_le_bytes());
        }
    }

    pub fn load(&mut self, inp: &mut &[u8]) -> Option<()> {
        let take = |inp: &mut &[u8], n: usize| -> Option<Vec<u8>> {
            if inp.len() < n {
                return None;
            }
            let (a, b) = inp.split_at(n);
            *inp = b;
            Some(a.to_vec())
        };
        let h = take(inp, 10)?;
        self.dist_enabled = h[0] != 0;
        self.cpu_enabled = h[1] != 0;
        self.pmr = u32::from_le_bytes(h[2..6].try_into().ok()?);
        self.bpr = u32::from_le_bytes(h[6..10].try_into().ok()?);
        for i in 0..NUM_IRQS {
            let b = take(inp, 2)?;
            let f = b[0];
            self.enabled[i] = f & 1 != 0;
            self.latched[i] = f & 2 != 0;
            self.line[i] = f & 4 != 0;
            self.active[i] = f & 8 != 0;
            self.edge[i] = f & 16 != 0;
            self.group[i] = f & 32 != 0;
            self.priority[i] = b[1];
        }
        let n = u32::from_le_bytes(take(inp, 4)?.try_into().ok()?) as usize;
        self.active_stack.clear();
        for _ in 0..n {
            self.active_stack
                .push(u32::from_le_bytes(take(inp, 4)?.try_into().ok()?));
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_irq_flow() {
        let mut g = Gic::new();
        g.dist_write(0x000, 4, 1);
        g.cpu_write(0x00, 1);
        g.cpu_write(0x04, 0xf0);
        g.dist_write(0x100 + 4, 4, 1 << (40 - 32)); // enable SPI 40
        assert!(!g.irq_asserted());
        g.set_line(40, true);
        assert!(g.irq_asserted());
        assert_eq!(g.cpu_read(0x0c), 40);
        assert!(!g.irq_asserted(), "active interrupt must not re-signal");
        g.set_line(40, false);
        g.cpu_write(0x10, 40);
        assert!(!g.irq_asserted());
        assert_eq!(g.cpu_read(0x0c), SPURIOUS as u64);
    }

    #[test]
    fn priority_mask_blocks() {
        let mut g = Gic::new();
        g.dist_write(0x000, 4, 1);
        g.cpu_write(0x00, 1);
        g.cpu_write(0x04, 0x80);
        g.dist_write(0x400 + 27, 1, 0xa0);
        g.dist_write(0x100, 4, 1 << 27);
        g.set_line(27, true);
        assert!(!g.irq_asserted());
        g.cpu_write(0x04, 0xf0);
        assert!(g.irq_asserted());
    }
}
