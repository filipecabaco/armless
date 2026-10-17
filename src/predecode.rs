//! Pre-decoded instructions for the interpreter: per physical code page, which leaf of the A64
//! decoder each instruction word goes to. `step` then dispatches through one jump table instead of
//! walking the decode tree for every instruction it executes. The leaves are the same functions
//! `execute` reaches, so the result is the same; only the choice of leaf is remembered.
//!
//! Decoded pages live in the JIT's bookkeeping (`Jit::pre`) and are dropped wherever its code is:
//! a store to the page, an instruction-cache invalidation, a DMA write into it, a full flush.

use crate::board::Host;
use crate::cpu::{Cpu, Res};

/// Instructions in a 4 KiB page.
pub(crate) const PAGE_INSNS: usize = 1024;
/// Pages kept decoded (8 KiB each); past that every page is dropped and decoded afresh.
const MAX_PAGES: usize = 1024;
/// No slot.
pub(crate) const NONE: u32 = u32::MAX;

// Leaves dispatched to directly. 0 means the entry is not decoded yet; GENERIC runs `execute`.
// GENERIC and the branches come first: only they may set `next_pc` (see `may_branch`).
const GENERIC: u8 = 1;
const B_BL: u8 = 2;
const CBZ_CBNZ: u8 = 3;
const TBZ_TBNZ: u8 = 4;
const B_COND: u8 = 5;
const BRANCH_REG: u8 = 6;
const ADR: u8 = 7;
const ADD_SUB_IMM: u8 = 8;
const LOGICAL_IMM: u8 = 9;
const MOVE_WIDE: u8 = 10;
const BITFIELD_IMM: u8 = 11;
const EXTR: u8 = 12;
const LDR_LITERAL: u8 = 13;
const LDST_PAIR: u8 = 14;
const LDST_UIMM: u8 = 15;
const LDST_IMM9: u8 = 16;
const LDST_REGOFF: u8 = 17;
const LOGICAL_SHIFTED: u8 = 18;
const ADD_SUB_SHIFTED: u8 = 19;
const ADD_SUB_EXTENDED: u8 = 20;
const ADC_SBC: u8 = 21;
const COND_COMPARE: u8 = 22;
const COND_SELECT: u8 = 23;
const DP_2SRC: u8 = 24;
const DP_1SRC: u8 = 25;
const DP_3SRC: u8 = 26;

/// The leaf `execute` would reach for `insn`, following the same tests in the same order.
pub(crate) fn leaf_of(insn: u32) -> u8 {
    match (insn >> 25) & 0xf {
        0b1000 | 0b1001 => match (insn >> 23) & 7 {
            0b000 | 0b001 => ADR,
            0b010 => ADD_SUB_IMM,
            0b100 => LOGICAL_IMM,
            0b101 => MOVE_WIDE,
            0b110 => BITFIELD_IMM,
            _ => EXTR,
        },
        0b1010 | 0b1011 => {
            if insn & 0x7c00_0000 == 0x1400_0000 {
                B_BL
            } else if insn & 0x7e00_0000 == 0x3400_0000 {
                CBZ_CBNZ
            } else if insn & 0x7e00_0000 == 0x3600_0000 {
                TBZ_TBNZ
            } else if insn & 0xff00_0010 == 0x5400_0000 {
                B_COND
            } else if insn & 0xfe00_0000 == 0xd600_0000 {
                BRANCH_REG
            } else {
                // Exception generation, system instructions, unallocated
                GENERIC
            }
        },
        0b0100 | 0b0110 | 0b1100 | 0b1110 => {
            if insn & (1 << 26) != 0 || insn & 0x3f00_0000 == 0x0800_0000 {
                // SIMD&FP loads and stores, exclusives
                GENERIC
            } else if insn & 0x3b00_0000 == 0x1800_0000 {
                LDR_LITERAL
            } else if insn & 0x3800_0000 == 0x2800_0000 {
                LDST_PAIR
            } else if insn & 0x3800_0000 == 0x3800_0000 {
                if insn & (1 << 24) != 0 {
                    LDST_UIMM
                } else if insn & (1 << 21) == 0 {
                    LDST_IMM9
                } else if (insn >> 10) & 3 == 2 {
                    LDST_REGOFF
                } else {
                    GENERIC
                }
            } else {
                GENERIC
            }
        },
        0b0101 | 0b1101 => {
            if insn & 0x1f00_0000 == 0x0a00_0000 {
                LOGICAL_SHIFTED
            } else if insn & 0x1f20_0000 == 0x0b00_0000 {
                ADD_SUB_SHIFTED
            } else if insn & 0x1f20_0000 == 0x0b20_0000 {
                ADD_SUB_EXTENDED
            } else if insn & 0x1fe0_0000 == 0x1a00_0000 {
                ADC_SBC
            } else if insn & 0x1fe0_0000 == 0x1a40_0000 {
                COND_COMPARE
            } else if insn & 0x1fe0_0000 == 0x1a80_0000 {
                COND_SELECT
            } else if insn & 0x5fe0_0000 == 0x1ac0_0000 {
                DP_2SRC
            } else if insn & 0x5fe0_0000 == 0x5ac0_0000 {
                DP_1SRC
            } else if insn & 0x1f00_0000 == 0x1b00_0000 {
                DP_3SRC
            } else {
                GENERIC
            }
        },
        _ => GENERIC,
    }
}

/// Whether the instruction of entry `e` may set `next_pc`: a branch, or anything `execute` runs.
/// The others always go on to the next instruction (or raise an exception).
#[inline(always)]
pub(crate) fn may_branch(e: u64) -> bool {
    (e >> 32) as u8 <= BRANCH_REG
}

/// A decoded entry: the leaf above the instruction word. Never 0.
#[inline(always)]
pub(crate) fn entry(insn: u32) -> u64 {
    (leaf_of(insn) as u64) << 32 | insn as u64
}

/// The decoded pages of one CPU.
pub struct Cache {
    /// Physical page number -> slot, or NONE.
    page_slot: Vec<u32>,
    /// Slot -> its page's entries (0 = not decoded yet).
    slots: Vec<Box<[u64]>>,
    slot_page: Vec<u32>,
    free: Vec<u32>,
    /// The page `step` executes from: its physical base and its entries. Reset when any slot
    /// is dropped, so `step` never runs entries of a dropped page.
    pub(crate) cur_base: usize,
    pub(crate) cur_ptr: *mut u64,
    /// Statistics: pages decoded, pages dropped, full flushes.
    pub decoded: u64,
    pub dropped: u64,
    pub flushes: u64,
}

impl Cache {
    pub fn new(memory_size: usize) -> Cache {
        Cache {
            page_slot: vec![NONE; memory_size.div_ceil(4096)],
            slots: Vec::new(),
            slot_page: Vec::new(),
            free: Vec::new(),
            cur_base: usize::MAX,
            cur_ptr: std::ptr::null_mut(),
            decoded: 0,
            dropped: 0,
            flushes: 0,
        }
    }

    #[inline(always)]
    pub(crate) fn slot(&self, page: usize) -> u32 {
        self.page_slot[page]
    }

    /// Make `slot`, the slot of the page at physical address `base`, the current page.
    pub(crate) fn set_current(&mut self, base: usize, slot: u32) {
        self.cur_base = base;
        // A slot's entries are a boxed slice: they stay put when `slots` grows.
        self.cur_ptr = self.slots[slot as usize].as_mut_ptr();
    }

    /// Entry `i` of the current page, if `base` is that page (0: not decoded yet).
    #[inline(always)]
    pub(crate) fn current(&self, base: usize, i: usize) -> u64 {
        debug_assert!(i < PAGE_INSNS);
        if base != self.cur_base {
            return 0;
        }
        // SAFETY: `cur_ptr` points at the entries of the page at `cur_base`, a live slot (dropping
        // any slot resets `cur_base`), and `i` is an instruction index within a page.
        unsafe { *self.cur_ptr.add(i) }
    }

    /// Record entry `i` of the current page.
    #[inline(always)]
    pub(crate) fn set_current_entry(&mut self, i: usize, e: u64) {
        debug_assert!(i < PAGE_INSNS && self.cur_base != usize::MAX);
        // SAFETY: as in `current`.
        unsafe { *self.cur_ptr.add(i) = e }
    }

    /// Entry `i` of `slot`.
    pub(crate) fn get(&self, slot: u32, i: usize) -> u64 {
        self.slots[slot as usize][i]
    }

    /// A slot for physical page `page`, with nothing decoded yet.
    pub(crate) fn alloc(&mut self, page: usize) -> u32 {
        if self.free.is_empty() && self.slots.len() >= MAX_PAGES {
            self.clear();
        }
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.slots.push(vec![0; PAGE_INSNS].into_boxed_slice());
                self.slot_page.push(NONE);
                (self.slots.len() - 1) as u32
            },
        };
        self.page_slot[page] = slot;
        self.slot_page[slot as usize] = page as u32;
        self.decoded += 1;
        slot
    }

    /// Forget the decoded instructions of physical page `page`, if any.
    pub(crate) fn drop_page(&mut self, page: usize) {
        let Some(&slot) = self.page_slot.get(page) else {
            return;
        };
        if slot == NONE {
            return;
        }
        self.page_slot[page] = NONE;
        self.slot_page[slot as usize] = NONE;
        self.slots[slot as usize].fill(0);
        self.free.push(slot);
        self.dropped += 1;
        self.cur_base = usize::MAX;
    }

    pub(crate) fn clear(&mut self) {
        for (slot, page) in self.slot_page.iter_mut().enumerate() {
            if *page != NONE {
                self.page_slot[*page as usize] = NONE;
                *page = NONE;
                self.slots[slot].fill(0);
                self.free.push(slot as u32);
            }
        }
        self.flushes += 1;
        self.cur_base = usize::MAX;
    }
}

impl<H: Host> Cpu<H> {
    /// Execute a decoded entry: the leaf `execute` would reach, directly.
    #[inline(always)]
    pub(crate) fn exec_decoded(&mut self, e: u64) -> Res<()> {
        let insn = e as u32;
        match (e >> 32) as u8 {
            ADR => self.adr(insn),
            ADD_SUB_IMM => self.add_sub_imm(insn),
            LOGICAL_IMM => self.logical_imm(insn),
            MOVE_WIDE => self.move_wide(insn),
            BITFIELD_IMM => self.bitfield_imm(insn),
            EXTR => self.extr(insn),
            B_BL => self.b_bl(insn),
            CBZ_CBNZ => self.cbz_cbnz(insn),
            TBZ_TBNZ => self.tbz_tbnz(insn),
            B_COND => self.b_cond(insn),
            BRANCH_REG => self.branch_reg(insn),
            LDR_LITERAL => self.ldr_literal(insn),
            LDST_PAIR => self.ldst_pair(insn),
            LDST_UIMM => self.ldst_uimm(insn),
            LDST_IMM9 => self.ldst_imm9(insn),
            LDST_REGOFF => self.ldst_regoff(insn),
            LOGICAL_SHIFTED => self.logical_shifted(insn),
            ADD_SUB_SHIFTED => self.add_sub_shifted(insn),
            ADD_SUB_EXTENDED => self.add_sub_extended(insn),
            ADC_SBC => self.adc_sbc(insn),
            COND_COMPARE => self.cond_compare(insn),
            COND_SELECT => self.cond_select(insn),
            DP_2SRC => self.dp_2src_leaf(insn),
            DP_1SRC => self.dp_1src_leaf(insn),
            DP_3SRC => self.dp_3src_leaf(insn),
            _ => self.execute(insn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    const MOVZ_X0: u32 = 0xd280_0000; // movz x0, #imm16 (imm in bits 5..21)
    const ADD_X0_1: u32 = 0x9100_0400; // add x0, x0, #1
    const STR_W2_X3: u32 = 0xb900_0062; // str w2, [x3]

    fn cpu() -> Cpu<NoDevices> {
        Cpu::new(8 << 20, NoDevices)
    }

    fn put(c: &mut Cpu<NoDevices>, addr: usize, insn: u32) {
        c.mem[addr..addr + 4].copy_from_slice(&insn.to_le_bytes());
    }

    /// Runs the one instruction at `pc`.
    fn run_at(c: &mut Cpu<NoDevices>, pc: u64) {
        c.pc = pc;
        c.run(1);
    }

    #[test]
    fn the_leaf_of_an_instruction_is_the_one_execute_reaches() {
        assert_eq!(leaf_of(ADD_X0_1), ADD_SUB_IMM);
        assert_eq!(leaf_of(STR_W2_X3), LDST_UIMM);
        assert_eq!(leaf_of(0x5400_0001), B_COND); // b.ne
        assert_eq!(leaf_of(0xd65f_03c0), BRANCH_REG); // ret
        assert_eq!(leaf_of(0xd503_201f), GENERIC); // nop: a system instruction
        assert_eq!(leaf_of(0x4ea1_1c20), GENERIC); // mov v0.16b, v1.16b: SIMD
        assert_ne!(
            entry(0),
            0,
            "an entry is never mistaken for an undecoded one"
        );
    }

    #[test]
    fn a_store_over_decoded_code_is_executed_as_written() {
        let mut c = cpu();
        put(&mut c, 0x1000, MOVZ_X0 | 1 << 5);
        run_at(&mut c, 0x1000);
        assert_eq!(c.x[0], 1);
        // The guest rewrites the instruction it already ran.
        put(&mut c, 0x2000, STR_W2_X3);
        c.x[2] = (MOVZ_X0 | 2 << 5) as u64;
        c.x[3] = 0x1000;
        run_at(&mut c, 0x2000);
        run_at(&mut c, 0x1000);
        assert_eq!(c.x[0], 2);
    }

    #[test]
    fn a_device_writing_over_decoded_code_is_seen() {
        let mut c = cpu();
        put(&mut c, 0x1000, MOVZ_X0 | 1 << 5);
        run_at(&mut c, 0x1000);
        let io = crate::dev::Io::new(c.machine.clone(), || 0);
        assert!(io.write(0x1000, &(MOVZ_X0 | 3 << 5).to_le_bytes()));
        run_at(&mut c, 0x1000);
        assert_eq!(c.x[0], 3);
    }

    #[test]
    fn a_device_writing_the_same_decoded_page_again_is_seen_again() {
        let mut c = cpu();
        let io = crate::dev::Io::new(c.machine.clone(), || 0);
        put(&mut c, 0x1000, MOVZ_X0 | 1 << 5);
        run_at(&mut c, 0x1000);
        for n in 2..5u32 {
            // The page was decoded again since the last write: this write must drop it too.
            assert!(io.write(0x1000, &(MOVZ_X0 | n << 5).to_le_bytes()));
            run_at(&mut c, 0x1000);
            assert_eq!(c.x[0], n as u64);
        }
    }

    #[test]
    fn more_code_pages_than_are_kept_still_run_correctly() {
        let mut c = cpu();
        let pages = MAX_PAGES + 10;
        for p in 0..pages {
            put(&mut c, 0x1000 * (p + 1), ADD_X0_1);
        }
        for round in 1..=2 {
            for p in 0..pages {
                run_at(&mut c, 0x1000 * (p as u64 + 1));
            }
            assert_eq!(c.x[0], (round * pages) as u64);
        }
        assert!(c.jit.pre.flushes > 0);
    }
}
