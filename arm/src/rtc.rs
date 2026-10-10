//! PL031 real-time clock, so the guest gets wall-clock time at boot
//! (needed for TLS certificate checks in snowglobe's HTTPS bridge).

#[derive(Clone, Default)]
pub struct Rtc {
    /// Guest time minus host time, in seconds (set through RTCLR).
    pub offset: i64,
    pub match_value: u32,
    pub enabled: bool,
    pub irq_mask: bool,
    pub raw_irq: bool,
}

const PRIMECELL_ID: [u8; 8] = [0x31, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

impl Rtc {
    fn now(&self, host_secs: u64) -> u32 {
        (host_secs as i64).wrapping_add(self.offset) as u32
    }

    pub fn read(&mut self, off: u64, host_secs: u64) -> u64 {
        (match off {
            0x00 => self.now(host_secs),
            0x04 => self.match_value,
            0x08 => self.now(host_secs),
            0x0c => 1, // always running
            0x10 => self.irq_mask as u32,
            0x14 => self.raw_irq as u32,
            0x18 => (self.raw_irq && self.irq_mask) as u32,
            0xfe0..=0xffc => PRIMECELL_ID[((off - 0xfe0) >> 2) as usize] as u32,
            _ => 0,
        }) as u64
    }

    /// Returns the new level of the RTC interrupt line.
    pub fn write(&mut self, off: u64, value: u64, host_secs: u64) -> bool {
        let v = value as u32;
        match off {
            0x04 => self.match_value = v,
            0x08 => self.offset = v as i64 - host_secs as i64,
            0x0c => self.enabled = true,
            0x10 => self.irq_mask = v & 1 != 0,
            0x1c => self.raw_irq = false,
            _ => {}
        }
        self.raw_irq && self.irq_mask
    }
}
