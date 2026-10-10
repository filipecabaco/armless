//! Stage-1 EL1&0 address translation: VMSAv8-64, 4 KiB granule, with a
//! direct-mapped software TLB. Invalidation is coarse (any TLBI, or a write to
//! SCTLR/TCR/TTBRx, flushes everything), which is simple and always correct.

use crate::board::Host;
use crate::cpu::{ec, Cpu, Event, Res};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
    Exec,
}

const TLB_SIZE: usize = 4096;

// Permission bits cached per TLB entry.
const P_R0: u8 = 1;
const P_W0: u8 = 2;
const P_X0: u8 = 4;
const P_R1: u8 = 8;
const P_W1: u8 = 16;
const P_X1: u8 = 32;

#[derive(Clone, Copy)]
struct Entry {
    /// VA >> 12 (with the top byte as used), or u64::MAX when invalid.
    tag: u64,
    pa_page: u64,
    perms: u8,
    level: u8,
}

pub struct Tlb {
    entries: Vec<Entry>,
}

impl Default for Tlb {
    fn default() -> Self {
        Self::new()
    }
}

impl Tlb {
    pub fn new() -> Tlb {
        Tlb {
            entries: vec![
                Entry {
                    tag: u64::MAX,
                    pa_page: 0,
                    perms: 0,
                    level: 0
                };
                TLB_SIZE
            ],
        }
    }

    pub fn flush(&mut self) {
        for e in self.entries.iter_mut() {
            e.tag = u64::MAX;
        }
    }
}

/// Outcome of a table walk.
pub struct Walk {
    pub pa: u64,
    pub perms: u8,
    pub level: u8,
}

/// Fault status codes (DFSC/IFSC).
#[derive(Debug, Clone, Copy)]
pub enum Fault {
    Translation(u8),
    AccessFlag(u8),
    Permission(u8),
}

impl Fault {
    pub fn fsc(self) -> u32 {
        match self {
            Fault::Translation(l) => 0x04 | l as u32,
            Fault::AccessFlag(l) => 0x08 | l as u32,
            Fault::Permission(l) => 0x0c | l as u32,
        }
    }
}

impl<H: Host> Cpu<H> {
    pub fn mmu_enabled(&self) -> bool {
        self.sys.sctlr & 1 != 0
    }

    /// Translate a virtual address, raising the appropriate abort on failure.
    pub fn translate(&mut self, va: u64, acc: Access, unpriv: bool) -> Res<u64> {
        if !self.mmu_enabled() {
            return Ok(va);
        }
        let el0 = self.el == 0 || unpriv;
        let need = match (acc, el0) {
            (Access::Read, true) => P_R0,
            (Access::Write, true) => P_W0,
            (Access::Exec, true) => P_X0,
            (Access::Read, false) => P_R1,
            (Access::Write, false) => P_W1,
            (Access::Exec, false) => P_X1,
        };
        let key = self.tlb_key(va, acc);
        let idx = (key as usize) & (TLB_SIZE - 1);
        let e = self.tlb.entries[idx];
        if e.tag == key {
            if e.perms & need != 0 {
                return Ok((e.pa_page << 12) | (va & 0xfff));
            }
            return Err(self.abort(va, acc, Fault::Permission(e.level)));
        }
        match self.walk(va, acc) {
            Ok(w) => {
                self.tlb.entries[idx] = Entry {
                    tag: key,
                    pa_page: w.pa >> 12,
                    perms: w.perms,
                    level: w.level,
                };
                if w.perms & need != 0 {
                    Ok(w.pa)
                } else {
                    Err(self.abort(va, acc, Fault::Permission(w.level)))
                }
            }
            Err(f) => Err(self.abort(va, acc, f)),
        }
    }

    /// The TLB key: the VA page, ignoring the top byte where TBI applies.
    fn tlb_key(&self, va: u64, acc: Access) -> u64 {
        self.strip_tag(va, acc) >> 12
    }

    fn strip_tag(&self, va: u64, acc: Access) -> u64 {
        if acc == Access::Exec {
            return va;
        }
        let upper = va & (1 << 55) != 0;
        let tbi = if upper {
            self.sys.tcr & (1 << 38) != 0
        } else {
            self.sys.tcr & (1 << 37) != 0
        };
        if tbi {
            if upper {
                va | 0xff00_0000_0000_0000
            } else {
                va & 0x00ff_ffff_ffff_ffff
            }
        } else {
            va
        }
    }

    fn abort(&self, va: u64, acc: Access, f: Fault) -> Event {
        let lower = self.el == 0;
        let (class, wnr) = match acc {
            Access::Exec => (if lower { ec::IABT_LOW } else { ec::IABT_CUR }, 0),
            Access::Read => (if lower { ec::DABT_LOW } else { ec::DABT_CUR }, 0),
            Access::Write => (if lower { ec::DABT_LOW } else { ec::DABT_CUR }, 1 << 6),
        };
        Event::Sync {
            ec: class,
            iss: f.fsc() | wnr,
            far: Some(va),
            ret_next: false,
        }
    }

    /// Walk the translation tables for `va`.
    pub fn walk(&mut self, va: u64, acc: Access) -> Result<Walk, Fault> {
        let va = self.strip_tag(va, acc);
        let tcr = self.sys.tcr;
        let upper = va & (1 << 55) != 0;
        let (tsz, epd, tg, ttbr) = if upper {
            ((tcr >> 16) & 0x3f, tcr & (1 << 23) != 0, (tcr >> 30) & 3, self.sys.ttbr1)
        } else {
            (tcr & 0x3f, tcr & (1 << 7) != 0, (tcr >> 14) & 3, self.sys.ttbr0)
        };
        // Only the 4K granule is implemented (TG0=0b00, TG1=0b10).
        let tg_ok = if upper { tg == 2 } else { tg == 0 };
        let tsz = tsz.clamp(16, 39) as u32;
        let inputsize = 64 - tsz;
        if epd || !tg_ok {
            return Err(Fault::Translation(0));
        }
        // Bits above the input range must all equal bit 55's side.
        let top = va >> inputsize;
        let expect = if upper { u64::MAX >> inputsize } else { 0 };
        if top != expect {
            return Err(Fault::Translation(0));
        }

        let start_level: u32 = 4 - (inputsize - 12).div_ceil(9);
        let va_in = va & crate::bits::ones(inputsize);
        let mut table = ttbr & 0x0000_ffff_ffff_fffe;
        let mut ap_table: u64 = 0;
        let mut uxn_table = false;
        let mut pxn_table = false;
        let mut level = start_level;
        loop {
            let shift = 12 + 9 * (3 - level);
            let index = (va_in >> shift) & 0x1ff;
            let desc_addr = table.wrapping_add(index * 8);
            let desc = match self.phys_read(desc_addr, 8) {
                Some(d) if (desc_addr as usize) < self.mem.len() => d,
                _ => return Err(Fault::Translation(level as u8)),
            };
            if desc & 1 == 0 {
                return Err(Fault::Translation(level as u8));
            }
            let is_table = desc & 2 != 0;
            if level < 3 && is_table {
                ap_table |= (desc >> 61) & 3;
                uxn_table |= desc & (1 << 60) != 0;
                pxn_table |= desc & (1 << 59) != 0;
                table = desc & 0x0000_ffff_ffff_f000;
                level += 1;
                continue;
            }
            if level == 3 && !is_table {
                return Err(Fault::Translation(3));
            }
            if level == 0 {
                return Err(Fault::Translation(0));
            }
            // Block (level 1/2) or page (level 3).
            let out_mask = crate::bits::ones(shift);
            let base = desc & 0x0000_ffff_ffff_f000 & !out_mask;
            let pa = base | (va & out_mask);
            if desc & (1 << 10) == 0 {
                return Err(Fault::AccessFlag(level as u8));
            }
            let ap = (desc >> 6) & 3;
            let mut el0_access = ap & 1 != 0;
            let mut read_only = ap & 2 != 0;
            // Hierarchical APTable restrictions.
            if ap_table & 1 != 0 {
                el0_access = false;
            }
            if ap_table & 2 != 0 {
                read_only = true;
            }
            let uxn = desc & (1 << 54) != 0 || uxn_table;
            let mut pxn = desc & (1 << 53) != 0 || pxn_table;
            // EL1 may not execute memory that EL0 can write.
            if el0_access && !read_only {
                pxn = true;
            }
            let mut perms = P_R1;
            if !read_only {
                perms |= P_W1;
            }
            if !pxn {
                perms |= P_X1;
            }
            if el0_access {
                perms |= P_R0;
                if !read_only {
                    perms |= P_W0;
                }
            }
            // EL0 execute permission depends only on UXN (execute-only
            // mappings have AP=0b00 and UXN=0).
            if !uxn {
                perms |= P_X0;
            }
            // WXN: writable implies execute-never.
            if self.sys.sctlr & (1 << 19) != 0 && perms & P_W1 != 0 {
                perms &= !(P_X1 | P_X0);
            }
            return Ok(Walk {
                pa,
                perms,
                level: level as u8,
            });
        }
    }

    pub fn tlb_flush(&mut self) {
        self.tlb.flush();
        self.fast[0].flush();
        self.fast[1].flush();
    }

    /// AT S1E{0,1}{R,W}: translate and report through PAR_EL1.
    pub(crate) fn address_translate(&mut self, va: u64, write: bool, el0: bool) {
        if !self.mmu_enabled() {
            self.sys.par = va & 0x0000_ffff_ffff_f000;
            return;
        }
        let acc = if write { Access::Write } else { Access::Read };
        let need = match (write, el0) {
            (false, true) => P_R0,
            (true, true) => P_W0,
            (false, false) => P_R1,
            (true, false) => P_W1,
        };
        self.sys.par = match self.walk(va, acc) {
            Ok(w) if w.perms & need != 0 => w.pa & 0x0000_ffff_ffff_f000,
            Ok(w) => 1 | ((Fault::Permission(w.level).fsc() as u64) << 1),
            Err(f) => 1 | ((f.fsc() as u64) << 1),
        };
    }
}
