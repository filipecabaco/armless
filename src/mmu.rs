//! Stage-1 EL1&0 address translation: VMSAv8-64, 4 KiB granule, with a
//! direct-mapped software TLB.
//!
//! Entries are tagged as the architecture tags them: with the ASID they were made under, unless
//! the mapping is global (nG clear: the kernel's). So a TTBR write, a context switch to another
//! ASID, keeps them, as hardware does; the guest invalidates what it changes with TLBI. TLBI by VA
//! of a user page drops that page (whatever its ASID) when no user block mappings are cached;
//! every other TLBI, and writes to SCTLR and TCR, flush everything. (Invalidating more than asked
//! is always correct.) The per-EL fast tables in front of it are not tagged, so any TTBR write
//! drops those, and they refill from here.

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
    /// Matches under any ASID (a global mapping), else only under `asid`.
    global: bool,
    asid: u16,
}

pub struct Tlb {
    entries: Vec<Entry>,
    /// Cached entries for user (TTBR0) addresses that come from block
    /// mappings: those cover many 4 KiB slots, so per-page TLBI can't be used.
    user_blocks: u32,
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
                    level: 0,
                    global: false,
                    asid: 0,
                };
                TLB_SIZE
            ],
            user_blocks: 0,
        }
    }

    pub fn flush(&mut self) {
        for e in self.entries.iter_mut() {
            e.tag = u64::MAX;
        }
        self.user_blocks = 0;
    }
}

/// Page number ignoring the top byte (TBI) part of a TLB tag.
const PAGE_MASK: u64 = 0x000f_ffff_ffff_ffff;

/// Outcome of a table walk.
pub struct Walk {
    pub pa: u64,
    pub perms: u8,
    pub level: u8,
    /// nG was clear: the translation holds under every ASID.
    pub global: bool,
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
        let asid = self.asid();
        if e.tag == key && (e.global || e.asid == asid) {
            if e.perms & need != 0 {
                return Ok((e.pa_page << 12) | (va & 0xfff));
            }
            return Err(self.abort(va, acc, Fault::Permission(e.level)));
        }
        match self.walk(va, acc) {
            Ok(w) => {
                if w.level < 3 && va & (1 << 55) == 0 {
                    self.tlb.user_blocks += 1;
                }
                self.tlb.entries[idx] = Entry {
                    tag: key,
                    pa_page: w.pa >> 12,
                    perms: w.perms,
                    level: w.level,
                    global: w.global,
                    asid,
                };
                if w.perms & need != 0 {
                    Ok(w.pa)
                } else {
                    Err(self.abort(va, acc, Fault::Permission(w.level)))
                }
            },
            Err(f) => Err(self.abort(va, acc, f)),
        }
    }

    /// The current ASID: TTBR1's if TCR.A1, else TTBR0's; 16 bits if TCR.AS, else 8.
    pub(crate) fn asid(&self) -> u16 {
        let tcr = self.sys.tcr;
        let ttbr = if tcr & (1 << 22) != 0 { self.sys.ttbr1 } else { self.sys.ttbr0 };
        let asid = (ttbr >> 48) as u16;
        if tcr & (1 << 36) != 0 {
            asid
        } else {
            asid & 0xff
        }
    }

    /// A TTBR write: the main TLB keeps its entries, tagged with their ASIDs (the guest
    /// invalidates what it changes); the untagged fast tables are dropped.
    pub fn ttbr_changed(&mut self) {
        self.fast[0].flush();
        self.fast[1].flush();
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
        let tbi = if upper { self.sys.tcr & (1 << 38) != 0 } else { self.sys.tcr & (1 << 37) != 0 };
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
            (
                (tcr >> 16) & 0x3f,
                tcr & (1 << 23) != 0,
                (tcr >> 30) & 3,
                self.sys.ttbr1,
            )
        } else {
            (
                tcr & 0x3f,
                tcr & (1 << 7) != 0,
                (tcr >> 14) & 3,
                self.sys.ttbr0,
            )
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
                // nG (bit 11)
                global: desc & (1 << 11) == 0,
            });
        }
    }

    pub fn tlb_flush(&mut self) {
        self.tlb.flush();
        self.fast[0].flush();
        self.fast[1].flush();
    }

    /// TLBI by VA: `page` is the canonical VA >> 12. Drops the translations
    /// of that page only when that is enough, else everything.
    pub fn tlb_flush_page(&mut self, page: u64) {
        let user = page & (1 << 43) == 0;
        if !user || self.tlb.user_blocks != 0 {
            self.tlb_flush();
            return;
        }
        let idx = page as usize & (TLB_SIZE - 1);
        let e = &mut self.tlb.entries[idx];
        if e.tag != u64::MAX && e.tag & PAGE_MASK == page & PAGE_MASK {
            e.tag = u64::MAX;
        }
        for f in self.fast.iter_mut() {
            f.forget_page(page & PAGE_MASK, PAGE_MASK);
        }
    }

    /// The canonical VA page named by a TLBI operand (`VA[55:12]` in bits 43:0).
    pub fn tlbi_operand_page(op: u64) -> u64 {
        let page = op & 0x0fff_ffff_ffff;
        if page & (1 << 43) != 0 {
            page | 0xfff0_0000_0000_0000
        } else {
            page
        }
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

#[cfg(test)]
mod tests {
    use super::Access;
    use crate::board::Host;
    use crate::cpu::Cpu;

    struct NoDevices;
    impl Host for NoDevices {
        fn mmio_read(&mut self, _: u64, _: u32) -> Option<u64> {
            None
        }
        fn mmio_write(&mut self, _: u64, _: u32, _: u64) -> bool {
            false
        }
        fn now_ns(&mut self) -> u64 {
            0
        }
        fn wall_clock_secs(&mut self) -> u64 {
            0
        }
    }

    const VA: u64 = 0x4000_1000;
    // Valid, page, access flag, EL0 and EL1 read/write (AP = 01)
    const PAGE: u64 = 3 | (1 << 10) | (1 << 6);
    const NG: u64 = 1 << 11;

    /// Tables at `base` (level 1, 2, 3 in consecutive pages) mapping VA to `pa`, and the VA one
    /// page up (VA + 0x1000) to `global_pa`, global.
    fn tables(c: &Cpu<NoDevices>, base: u64, pa: u64, global_pa: u64) {
        let w = |at: u64, v: u64| c.mem.write(at as usize, &v.to_le_bytes());
        w(base + 8 * (VA >> 30), (base + 0x1000) | 3);
        w(
            base + 0x1000 + 8 * ((VA >> 21) & 0x1ff),
            (base + 0x2000) | 3,
        );
        w(base + 0x2000 + 8 * ((VA >> 12) & 0x1ff), pa | PAGE | NG);
        w(
            base + 0x2000 + 8 * (((VA + 0x1000) >> 12) & 0x1ff),
            global_pa | PAGE,
        );
    }

    #[test]
    fn tlb_entries_are_tagged_with_their_asid_unless_global() {
        let mut c = Cpu::new(16 << 20, NoDevices);
        // 4 KiB granule, 39-bit VA for TTBR0, start at level 1; 8-bit ASIDs from TTBR0
        c.sys.tcr = 25 | (25 << 16) | (2 << 30);
        c.sys.sctlr = 1;
        tables(&c, 0x10_0000, 0x20_0000, 0x30_0000);
        tables(&c, 0x18_0000, 0x21_0000, 0x30_0000);
        let as1 = 0x10_0000 | (1 << 48);
        let as2 = 0x18_0000 | (2 << 48);
        let at = |c: &mut Cpu<NoDevices>, va| c.translate(va, Access::Read, false).ok();
        for round in 0..3 {
            c.write_sysreg(crate::sysreg::TTBR0_EL1, as1).unwrap();
            assert_eq!(at(&mut c, VA), Some(0x20_0000), "ASID 1, round {round}");
            assert_eq!(at(&mut c, VA + 0x1000), Some(0x30_0000));
            // Another process: its own mapping of VA, not ASID 1's cached one
            c.write_sysreg(crate::sysreg::TTBR0_EL1, as2).unwrap();
            assert_eq!(at(&mut c, VA), Some(0x21_0000), "ASID 2, round {round}");
            assert_eq!(at(&mut c, VA + 0x1000), Some(0x30_0000));
        }
        // 16-bit ASIDs (TCR.AS): ASIDs that agree in their low byte are still different
        c.sys.tcr |= 1 << 36;
        c.write_sysreg(crate::sysreg::TCR_EL1, c.sys.tcr).unwrap();
        c.write_sysreg(crate::sysreg::TTBR0_EL1, 0x10_0000 | (0x101 << 48))
            .unwrap();
        assert_eq!(at(&mut c, VA), Some(0x20_0000));
        c.write_sysreg(crate::sysreg::TTBR0_EL1, 0x18_0000 | (0x201 << 48))
            .unwrap();
        assert_eq!(at(&mut c, VA), Some(0x21_0000));
        // ASID from TTBR1 (TCR.A1)
        c.sys.tcr |= 1 << 22;
        c.write_sysreg(crate::sysreg::TCR_EL1, c.sys.tcr).unwrap();
        c.write_sysreg(crate::sysreg::TTBR1_EL1, 7 << 48).unwrap();
        c.write_sysreg(crate::sysreg::TTBR0_EL1, 0x10_0000).unwrap();
        assert_eq!(at(&mut c, VA), Some(0x20_0000));
        c.write_sysreg(crate::sysreg::TTBR1_EL1, 8 << 48).unwrap();
        c.write_sysreg(crate::sysreg::TTBR0_EL1, 0x18_0000).unwrap();
        assert_eq!(at(&mut c, VA), Some(0x21_0000));
    }
}
