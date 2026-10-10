//! Translate the hot entry points of one physical code page into a wasm
//! function.
//!
//! Each entry point gets the basic block starting there. The function takes
//! the virtual PC of an entry, dispatches to it with a `br_table` and runs
//! blocks until control leaves the compiled entries of the page: direct
//! branches to another compiled entry on the same page loop back to the
//! dispatch without returning, as long as the instruction budget
//! (`Layout::limit`) lasts and no interrupt state changed (`irq_dirty`).
//! It returns a status:
//!   0 = done, cpu.pc holds the next PC
//!   1 = an interpreted instruction redirected control (cpu.pc holds it)
//!   2 = an exception or stop is pending (cpu.pc = the faulting instruction)
//!   3 = WFI executed (cpu.pc = the next instruction)
//!
//! Guest registers live in the CPU struct in linear memory and are read and
//! written in place, so state is precise at every instruction boundary.
//! Instructions without a translation call back into the interpreter.

use super::wasm::{self, Code, FuncType, Import, EMPTY, I32, I64};
use crate::bits::{bit, decode_bit_masks, field, sext};
use crate::sysreg::sr;

/// Linear-memory addresses the generated code uses (all wasm32 pointers).
#[derive(Clone, Copy, Debug, Default)]
pub struct Layout {
    pub x: u32,
    /// The stack pointer selected by this block's EL and SPSel.
    pub sp: u32,
    pub pc: u32,
    pub n: u32,
    pub z: u32,
    pub c: u32,
    pub v: u32,
    pub insn_count: u32,
    pub pending: u32,
    /// Non-zero while the exclusive monitor is armed: stores must go
    /// through the helper so they clear it.
    pub excl_armed: u32,
    /// u64: blocks stop chaining once insn_count reaches this.
    pub limit: u32,
    /// u64: physical address watched by the exclusive monitor.
    pub excl_addr: u32,
    /// System registers translated inline (u64 each; DAIF is u32).
    pub tpidr_el0: u32,
    pub tpidr_el1: u32,
    pub tpidrro_el0: u32,
    pub sp_el0: u32,
    pub daif: u32,
    /// Values (not addresses): the exception level and SPSel of this code.
    pub el: u32,
    pub spsel: u32,
    /// Non-zero with several CPUs: exclusives and DSB stay interpreted.
    pub smp: u32,
    /// Byte: non-zero when interrupt state must be re-examined.
    pub irq_dirty: u32,
    /// Guest RAM base.
    pub mem: u32,
    /// Fast TLB arrays for this block's EL (u64 tags, u32 page offsets).
    pub read_tag: u32,
    pub read_base: u32,
    pub write_tag: u32,
    pub write_base: u32,
}

pub const STATUS_DONE: i32 = 0;
pub const STATUS_REDIRECT: i32 = 1;
pub const STATUS_PENDING: i32 = 2;

// Imported helpers (function indices).
const H_READ: u32 = 0;
const H_WRITE: u32 = 1;
const H_INTERP: u32 = 2;

pub fn imports() -> Vec<Import> {
    vec![
        Import {
            name: "armless_jit_read",
            ty: FuncType {
                params: vec![I64, I32],
                results: vec![I64],
            },
        },
        Import {
            name: "armless_jit_write",
            ty: FuncType {
                params: vec![I64, I32, I64],
                results: vec![],
            },
        },
        Import {
            name: "armless_jit_interp",
            ty: FuncType {
                params: vec![I32, I64],
                results: vec![I32],
            },
        },
    ]
}

// Locals: 0 = pc (param); 1..=8 i64 temps; 9..=12 i32 temps.
const L_PC: u32 = 0;
const T0: u32 = 1;
const T1: u32 = 2;
const T2: u32 = 3;
const T3: u32 = 4;
const T4: u32 = 5;
const VA: u32 = 6;
const PAGE: u32 = 7;
const DATA: u32 = 8;
const IDX: u32 = 9;
const ST: u32 = 10;

const LOCALS: &[(u32, u8)] = &[(8, I64), (4, I32)];

/// Whether an instruction ends a block: branches, exception generation and
/// WFI/WFE. Other system instructions don't: the interpreter helper ends the
/// block when one changed state that translated code depends on.
pub fn ends_block(insn: u32) -> bool {
    let op0 = (insn >> 25) & 0xf;
    if op0 != 0b1010 && op0 != 0b1011 {
        return false;
    }
    if insn & 0xffc0_0000 == 0xd500_0000 {
        return insn & 0xffff_f01f == 0xd503_201f && matches!(field(insn, 5, 7), 2 | 3);
    }
    true
}

struct Gen<'a> {
    l: &'a Layout,
    c: Code,
    /// Byte offset of the current instruction from the block start.
    off: i64,
    /// Index of the current instruction in the block.
    k: u32,
    /// Byte offsets (within the page) of the entries compiled in this module.
    entries: &'a [u16],
    /// Page offset of the entry being translated (L_PC is its virtual address).
    entry_off: i64,
    /// Branch depth of the dispatch loop from the entry's top level.
    depth_top: u32,
    /// Extra structured-control nesting at the current emission point.
    nest: u32,
}

impl Gen<'_> {
    // ------------------------------------------------------------- helpers

    fn pc_at(&mut self, delta: i64) {
        self.c.get(L_PC);
        if delta != 0 {
            self.c.i64c(delta);
            self.c.i64_add();
        }
    }

    fn count(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        self.c.i32c(0);
        self.c.i32c(0);
        self.c.i64_load(self.l.insn_count);
        self.c.i64c(n as i64);
        self.c.i64_add();
        self.c.i64_store(self.l.insn_count);
    }

    /// cpu.pc = value on stack (emitted by `f`).
    fn store_pc(&mut self, f: impl FnOnce(&mut Self)) {
        self.c.i32c(0);
        f(self);
        self.c.i64_store(self.l.pc);
    }

    /// Exit with `status`, counting `n` executed instructions.
    fn exit(&mut self, status: i32, n: u32) {
        self.count(n);
        self.c.i32c(status);
        self.c.ret();
    }

    /// Continue at L_PC + `delta`, counting `n` instructions: chain to the
    /// entry there if this module has one, else return to the run loop.
    fn goto(&mut self, delta: i64, n: u32) {
        let l = *self.l;
        let target = self.entry_off + delta;
        let chain =
            (0..0x1000).contains(&target) && self.entries.binary_search(&(target as u16)).is_ok();
        if !chain {
            self.store_pc(|g| g.pc_at(delta));
            self.exit(STATUS_DONE, n);
            return;
        }
        self.count(n);
        self.pc_at(delta);
        self.c.set(L_PC);
        self.c.i32c(0);
        self.c.i64_load(l.insn_count);
        self.c.i32c(0);
        self.c.i64_load(l.limit);
        self.c.i64_lt_u();
        self.c.i32c(0);
        self.c.i32_load8_u(l.irq_dirty);
        self.c.i32_eqz();
        self.c.i32_and();
        self.c.br_if(self.depth_top + self.nest);
        self.store_pc(|g| g.c.get(L_PC));
        self.c.i32c(STATUS_DONE);
        self.c.ret();
    }

    fn xr(&mut self, r: u32) {
        if r == 31 {
            self.c.i64c(0);
        } else {
            self.c.i32c(0);
            self.c.i64_load(self.l.x + 8 * r);
        }
    }

    fn xs(&mut self, r: u32) {
        if r == 31 {
            self.c.i32c(0);
            self.c.i64_load(self.l.sp);
        } else {
            self.xr(r);
        }
    }

    /// Write the value produced by `f` to Xr (31 = XZR: value discarded).
    fn wr(&mut self, r: u32, f: impl FnOnce(&mut Self)) {
        if r == 31 {
            f(self);
            self.c.drop_();
        } else {
            self.c.i32c(0);
            f(self);
            self.c.i64_store(self.l.x + 8 * r);
        }
    }

    /// Write to Xr where 31 is SP.
    fn ws(&mut self, r: u32, f: impl FnOnce(&mut Self)) {
        self.c.i32c(0);
        f(self);
        self.c
            .i64_store(if r == 31 { self.l.sp } else { self.l.x + 8 * r });
    }

    fn trunc32(&mut self, sf: bool) {
        if !sf {
            self.c.i64c(0xffff_ffff);
            self.c.i64_and();
        }
    }

    fn set_flag(&mut self, addr: u32, f: impl FnOnce(&mut Self)) {
        self.c.i32c(0);
        f(self);
        self.c.i32_store8(addr);
    }

    /// N and Z from the result in local `r` (already truncated), C = V = 0.
    fn flags_logical(&mut self, r: u32, sf: bool) {
        let l = *self.l;
        self.set_flag(l.n, |g| {
            g.c.get(r);
            g.c.i64c(if sf { 63 } else { 31 });
            g.c.i64_shr_u();
            g.c.i32_wrap_i64();
            g.c.i32c(1);
            g.c.i32_and();
        });
        self.set_flag(l.z, |g| {
            g.c.get(r);
            g.c.i64_eqz();
        });
        self.set_flag(l.c, |g| g.c.i32c(0));
        self.set_flag(l.v, |g| g.c.i32c(0));
    }

    /// NZCV for x +/- y = r (locals; x and y already truncated for 32-bit,
    /// r truncated, `wide` holds the untruncated 32-bit sum for the carry).
    fn flags_addsub(&mut self, x: u32, y: u32, r: u32, wide: u32, sub: bool, sf: bool) {
        let l = *self.l;
        let top = if sf { 63 } else { 31 };
        self.set_flag(l.n, |g| {
            g.c.get(r);
            g.c.i64c(top);
            g.c.i64_shr_u();
            g.c.i32_wrap_i64();
            g.c.i32c(1);
            g.c.i32_and();
        });
        self.set_flag(l.z, |g| {
            g.c.get(r);
            g.c.i64_eqz();
        });
        self.set_flag(l.c, |g| {
            if sub {
                g.c.get(x);
                g.c.get(y);
                g.c.i64_ge_u();
            } else if sf {
                g.c.get(r);
                g.c.get(x);
                g.c.i64_lt_u();
            } else {
                g.c.get(wide);
                g.c.i64c(32);
                g.c.i64_shr_u();
                g.c.i32_wrap_i64();
                g.c.i32c(1);
                g.c.i32_and();
            }
        });
        self.set_flag(l.v, |g| {
            // add: (x ^ r) & (y ^ r); sub: (x ^ y) & (x ^ r)
            if sub {
                g.c.get(x);
                g.c.get(y);
                g.c.i64_xor();
                g.c.get(x);
                g.c.get(r);
                g.c.i64_xor();
            } else {
                g.c.get(x);
                g.c.get(r);
                g.c.i64_xor();
                g.c.get(y);
                g.c.get(r);
                g.c.i64_xor();
            }
            g.c.i64_and();
            g.c.i64c(top);
            g.c.i64_shr_u();
            g.c.i32_wrap_i64();
            g.c.i32c(1);
            g.c.i32_and();
        });
    }

    /// Push the i32 truth value of condition `cond`.
    fn cond(&mut self, cond: u32) {
        let l = *self.l;
        let ld = |g: &mut Self, a: u32| {
            g.c.i32c(0);
            g.c.i32_load8_u(a);
        };
        match cond >> 1 {
            0 => ld(self, l.z),
            1 => ld(self, l.c),
            2 => ld(self, l.n),
            3 => ld(self, l.v),
            4 => {
                ld(self, l.c);
                ld(self, l.z);
                self.c.i32_eqz();
                self.c.i32_and();
            },
            5 => {
                ld(self, l.n);
                ld(self, l.v);
                self.c.i32_eq();
            },
            6 => {
                ld(self, l.n);
                ld(self, l.v);
                self.c.i32_eq();
                ld(self, l.z);
                self.c.i32_eqz();
                self.c.i32_and();
            },
            _ => self.c.i32c(1),
        }
        if cond & 1 != 0 && cond != 0xf {
            self.c.i32_eqz();
        }
    }

    /// Shift the i64 on the stack (LSL/LSR/ASR/ROR) by a constant at size `sf`.
    fn shift_const(&mut self, kind: u32, amount: u32, sf: bool) {
        if amount == 0 {
            return;
        }
        if sf {
            self.c.i64c(amount as i64);
            match kind {
                0 => self.c.i64_shl(),
                1 => self.c.i64_shr_u(),
                2 => self.c.i64_shr_s(),
                _ => self.c.i64_rotr(),
            }
        } else {
            match kind {
                0 => {
                    self.c.i64c(amount as i64);
                    self.c.i64_shl();
                    self.trunc32(false);
                },
                1 => {
                    self.trunc32(false);
                    self.c.i64c(amount as i64);
                    self.c.i64_shr_u();
                },
                2 => {
                    self.c.i64_extend32_s();
                    self.c.i64c(amount as i64);
                    self.c.i64_shr_s();
                    self.trunc32(false);
                },
                _ => {
                    // 32-bit rotate: (x >> a) | (x << (32 - a)) on the low word
                    self.trunc32(false);
                    self.c.set(T4);
                    self.c.get(T4);
                    self.c.i64c(amount as i64);
                    self.c.i64_shr_u();
                    self.c.get(T4);
                    self.c.i64c(32 - amount as i64);
                    self.c.i64_shl();
                    self.c.i64_or();
                    self.trunc32(false);
                },
            }
        }
    }

    // ------------------------------------------------------- memory access

    /// Leave the guest address in local VA; returns nothing.
    /// Load `size` bytes at VA into the stack as i64 (zero-extended).
    fn load(&mut self, size: u32) {
        let l = *self.l;
        self.tlb_probe(l.read_tag, size);
        self.c.if_(I64);
        {
            self.host_addr(l.read_base);
            match size {
                1 => self.c.i64_load8_u(l.mem),
                2 => self.c.i64_load16_u(l.mem),
                4 => self.c.i64_load32_u(l.mem),
                _ => self.c.i64_load(l.mem),
            }
        }
        self.c.else_();
        {
            self.c.get(VA);
            self.c.i32c(size as i32);
            self.c.call(H_READ);
            self.check_pending();
        }
        self.c.end();
    }

    /// Store the i64 in local DATA (low `size` bytes) at VA.
    fn store(&mut self, size: u32) {
        let l = *self.l;
        self.tlb_probe(l.write_tag, size);
        self.c.i32c(0);
        self.c.i32_load8_u(l.excl_armed);
        self.c.i32_eqz();
        self.c.i32_and();
        self.c.if_(EMPTY);
        {
            self.host_addr(l.write_base);
            self.c.get(DATA);
            match size {
                1 => self.c.i64_store8(l.mem),
                2 => self.c.i64_store16(l.mem),
                4 => self.c.i64_store32(l.mem),
                _ => self.c.i64_store(l.mem),
            }
        }
        self.c.else_();
        {
            self.c.get(VA);
            self.c.i32c(size as i32);
            self.c.get(DATA);
            self.c.call(H_WRITE);
            self.check_pending();
        }
        self.c.end();
    }

    /// Push (tag[idx] == VA >> 12) && ((VA & 0xfff) + size <= 0x1000); sets IDX and PAGE.
    fn tlb_probe(&mut self, tags: u32, size: u32) {
        self.c.get(VA);
        self.c.i64c(12);
        self.c.i64_shr_u();
        self.c.tee(PAGE);
        self.c.i32_wrap_i64();
        self.c.i32c(255);
        self.c.i32_and();
        self.c.tee(IDX);
        self.c.i32c(3);
        self.c.i32_shl();
        self.c.i64_load(tags);
        self.c.get(PAGE);
        self.c.i64_eq();
        self.c.get(VA);
        self.c.i32_wrap_i64();
        self.c.i32c(0xfff);
        self.c.i32_and();
        self.c.i32c(0x1000 - size as i32);
        self.c.i32_le_u();
        self.c.i32_and();
    }

    /// Push the i32 host offset (page base + VA & 0xfff), relative to `mem`.
    fn host_addr(&mut self, bases: u32) {
        self.c.get(IDX);
        self.c.i32c(2);
        self.c.i32_shl();
        self.c.i32_load(bases);
        self.c.get(VA);
        self.c.i32_wrap_i64();
        self.c.i32c(0xfff);
        self.c.i32_and();
        self.c.i32_add();
    }

    /// After a helper call: if an exception is pending, exit at this instruction.
    fn check_pending(&mut self) {
        let l = *self.l;
        self.c.i32c(0);
        self.c.i32_load8_u(l.pending);
        self.c.if_(EMPTY);
        let off = self.off;
        self.store_pc(|g| g.pc_at(off));
        let k = self.k;
        self.exit(STATUS_PENDING, k);
        self.c.end();
    }

    // -------------------------------------------------------- instructions

    /// Emit an interpreter call for one instruction; exits on non-zero status.
    fn interp(&mut self, insn: u32) {
        self.c.i32c(insn as i32);
        let off = self.off;
        self.pc_at(off);
        self.c.call(H_INTERP);
        self.c.tee(ST);
        self.c.if_(EMPTY);
        let k = self.k + 1;
        self.count(k);
        self.c.get(ST);
        self.c.ret();
        self.c.end();
    }

    /// Translate one instruction. Returns false if it needs the interpreter.
    fn insn(&mut self, insn: u32) -> bool {
        match (insn >> 25) & 0xf {
            0b1000 | 0b1001 => {
                let start = self.c.b.len();
                self.dp_imm(insn) || {
                    self.c.b.truncate(start);
                    self.dp_extra(insn)
                }
            },
            0b0101 | 0b1101 => {
                let start = self.c.b.len();
                self.dp_reg(insn) || {
                    self.c.b.truncate(start);
                    self.dp_extra(insn)
                }
            },
            0b1010 | 0b1011 => self.branch(insn) || self.system(insn),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.ldst(insn),
            _ => false,
        }
    }

    fn dp_imm(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31);
        let rd = insn & 31;
        let rn = field(insn, 5, 5);
        match field(insn, 23, 3) {
            0b000 | 0b001 => {
                let immlo = field(insn, 29, 2) as u64;
                let immhi = field(insn, 5, 19) as u64;
                let imm = sext((immhi << 2) | immlo, 21);
                let off = self.off;
                if bit(insn, 31) {
                    self.wr(rd, |g| {
                        g.pc_at(off);
                        g.c.i64c(!0xfff);
                        g.c.i64_and();
                        g.c.i64c(imm << 12);
                        g.c.i64_add();
                    });
                } else {
                    self.wr(rd, |g| g.pc_at(off + imm));
                }
                true
            },
            0b010 => {
                let sub = bit(insn, 30);
                let set = bit(insn, 29);
                let mut imm = field(insn, 10, 12) as i64;
                if bit(insn, 22) {
                    imm <<= 12;
                }
                if !set {
                    self.ws(rd, |g| {
                        g.xs(rn);
                        g.c.i64c(imm);
                        if sub {
                            g.c.i64_sub();
                        } else {
                            g.c.i64_add();
                        }
                        g.trunc32(sf);
                    });
                    return true;
                }
                self.xs(rn);
                self.trunc32(sf);
                self.c.set(T0);
                self.c.i64c(imm);
                self.c.set(T1);
                self.addsub_flags(sub, sf, rd);
                true
            },
            0b100 => {
                let opc = field(insn, 29, 2);
                let nbit = field(insn, 22, 1);
                if !sf && nbit == 1 {
                    return false;
                }
                let Some(imm) = decode_bit_masks(nbit, field(insn, 10, 6), field(insn, 16, 6), sf)
                else {
                    return false;
                };
                let body = |g: &mut Self| {
                    g.xr(rn);
                    g.c.i64c(imm as i64);
                    match opc {
                        0 | 3 => g.c.i64_and(),
                        1 => g.c.i64_or(),
                        _ => g.c.i64_xor(),
                    }
                    g.trunc32(sf);
                };
                if opc == 3 {
                    body(self);
                    self.c.set(T2);
                    self.flags_logical(T2, sf);
                    self.wr(rd, |g| g.c.get(T2));
                } else {
                    self.ws(rd, body);
                }
                true
            },
            0b101 => {
                let opc = field(insn, 29, 2);
                let hw = field(insn, 21, 2);
                if !sf && hw > 1 {
                    return false;
                }
                let shift = hw * 16;
                let imm = (field(insn, 5, 16) as u64) << shift;
                match opc {
                    0 => {
                        let v = if sf { !imm } else { !imm & 0xffff_ffff };
                        self.wr(rd, |g| g.c.i64c(v as i64));
                    },
                    2 => self.wr(rd, |g| g.c.i64c(imm as i64)),
                    3 => self.wr(rd, |g| {
                        g.xr(rd);
                        g.c.i64c(!(0xffffu64 << shift) as i64);
                        g.c.i64_and();
                        g.c.i64c(imm as i64);
                        g.c.i64_or();
                        g.trunc32(sf);
                    }),
                    _ => return false,
                }
                true
            },
            0b110 => {
                // SBFM / UBFM (BFM is left to the interpreter)
                let opc = field(insn, 29, 2);
                if bit(insn, 22) != sf || opc == 1 || opc == 3 {
                    return false;
                }
                let immr = field(insn, 16, 6);
                let imms = field(insn, 10, 6);
                if !sf && (immr >= 32 || imms >= 32) {
                    return false;
                }
                let size = if sf { 64 } else { 32 };
                let signed = opc == 0;
                self.wr(rd, |g| {
                    // Move the field to the top of an i64, then shift it down
                    // (arithmetically for SBFM) into its final position.
                    g.xr(rn);
                    if imms >= immr {
                        // extract bits [imms:immr] to the bottom
                        let width = imms - immr + 1;
                        g.c.i64c((64 - (imms + 1)) as i64);
                        g.c.i64_shl();
                        g.c.i64c((64 - width) as i64);
                        if signed {
                            g.c.i64_shr_s();
                        } else {
                            g.c.i64_shr_u();
                        }
                    } else {
                        // insert the low imms+1 bits at size-immr
                        let width = imms + 1;
                        let pos = size - immr;
                        g.c.i64c((64 - width) as i64);
                        g.c.i64_shl();
                        g.c.i64c((64 - width - pos) as i64);
                        if signed {
                            g.c.i64_shr_s();
                        } else {
                            g.c.i64_shr_u();
                        }
                    }
                    g.trunc32(sf);
                });
                true
            },
            _ => false,
        }
    }

    /// ADDS/SUBS core: operands in T0 (x) and T1 (y), result to Rd.
    fn addsub_flags(&mut self, sub: bool, sf: bool, rd: u32) {
        if !sf {
            self.c.get(T1);
            self.trunc32(false);
            self.c.set(T1);
        }
        self.c.get(T0);
        self.c.get(T1);
        if sub {
            self.c.i64_sub();
        } else {
            self.c.i64_add();
        }
        self.c.tee(T3);
        self.trunc32(sf);
        self.c.set(T2);
        self.flags_addsub(T0, T1, T2, T3, sub, sf);
        self.wr(rd, |g| g.c.get(T2));
    }

    fn dp_reg(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31);
        let rd = insn & 31;
        let rn = field(insn, 5, 5);
        let rm = field(insn, 16, 5);
        if insn & 0x1f00_0000 == 0x0a00_0000 {
            // Logical (shifted register)
            let opc = field(insn, 29, 2);
            let imm6 = field(insn, 10, 6);
            if !sf && imm6 >= 32 {
                return false;
            }
            let kind = field(insn, 22, 2);
            let invert = bit(insn, 21);
            let body = |g: &mut Self| {
                g.xr(rn);
                g.xr(rm);
                g.shift_const(kind, imm6, sf);
                if invert {
                    g.c.i64c(-1);
                    g.c.i64_xor();
                }
                match opc {
                    0 | 3 => g.c.i64_and(),
                    1 => g.c.i64_or(),
                    _ => g.c.i64_xor(),
                }
                g.trunc32(sf);
            };
            if opc == 3 {
                body(self);
                self.c.set(T2);
                self.flags_logical(T2, sf);
                self.wr(rd, |g| g.c.get(T2));
            } else {
                self.wr(rd, body);
            }
            return true;
        }
        if insn & 0x1f20_0000 == 0x0b00_0000 {
            // ADD/SUB (shifted register)
            let kind = field(insn, 22, 2);
            let imm6 = field(insn, 10, 6);
            if kind == 3 || (!sf && imm6 >= 32) {
                return false;
            }
            let sub = bit(insn, 30);
            if !bit(insn, 29) {
                self.wr(rd, |g| {
                    g.xr(rn);
                    g.xr(rm);
                    g.shift_const(kind, imm6, sf);
                    if sub {
                        g.c.i64_sub();
                    } else {
                        g.c.i64_add();
                    }
                    g.trunc32(sf);
                });
                return true;
            }
            self.xr(rn);
            self.trunc32(sf);
            self.c.set(T0);
            self.xr(rm);
            self.shift_const(kind, imm6, sf);
            self.c.set(T1);
            self.addsub_flags(sub, sf, rd);
            return true;
        }
        if insn & 0x1f20_0000 == 0x0b20_0000 {
            // ADD/SUB (extended register)
            let imm3 = field(insn, 10, 3);
            if imm3 > 4 || field(insn, 22, 2) != 0 {
                return false;
            }
            let option = field(insn, 13, 3);
            let sub = bit(insn, 30);
            let set = bit(insn, 29);
            let ext = |g: &mut Self| {
                g.xr(rm);
                match option {
                    0 => {
                        g.c.i64c(0xff);
                        g.c.i64_and();
                    },
                    1 => {
                        g.c.i64c(0xffff);
                        g.c.i64_and();
                    },
                    2 => g.trunc32(false),
                    4 => g.c.i64_extend8_s(),
                    5 => g.c.i64_extend16_s(),
                    6 => g.c.i64_extend32_s(),
                    _ => {},
                }
                if imm3 != 0 {
                    g.c.i64c(imm3 as i64);
                    g.c.i64_shl();
                }
            };
            if !set {
                self.ws(rd, |g| {
                    g.xs(rn);
                    ext(g);
                    if sub {
                        g.c.i64_sub();
                    } else {
                        g.c.i64_add();
                    }
                    g.trunc32(sf);
                });
                return true;
            }
            self.xs(rn);
            self.trunc32(sf);
            self.c.set(T0);
            ext(self);
            self.c.set(T1);
            self.addsub_flags(sub, sf, rd);
            return true;
        }
        if insn & 0x1fe0_0000 == 0x1a80_0000 {
            // CSEL/CSINC/CSINV/CSNEG
            if bit(insn, 29) || bit(insn, 11) {
                return false;
            }
            let op = bit(insn, 30);
            let o2 = bit(insn, 10);
            let cond = field(insn, 12, 4);
            self.wr(rd, |g| {
                g.xr(rn);
                g.xr(rm);
                match (op, o2) {
                    (false, false) => {},
                    (false, true) => {
                        g.c.i64c(1);
                        g.c.i64_add();
                    },
                    (true, false) => {
                        g.c.i64c(-1);
                        g.c.i64_xor();
                    },
                    (true, true) => {
                        g.c.set(T4);
                        g.c.i64c(0);
                        g.c.get(T4);
                        g.c.i64_sub();
                    },
                }
                g.cond(cond);
                g.c.select();
                g.trunc32(sf);
            });
            return true;
        }
        if insn & 0x1fe0_0000 == 0x1a40_0000 {
            // CCMP/CCMN (register or immediate)
            if !bit(insn, 29) || bit(insn, 10) || bit(insn, 4) {
                return false;
            }
            let cond = field(insn, 12, 4);
            let sub = bit(insn, 30);
            let nzcv = insn & 0xf;
            let imm = bit(insn, 11);
            self.cond(cond);
            self.c.if_(EMPTY);
            {
                self.xr(rn);
                self.trunc32(sf);
                self.c.set(T0);
                if imm {
                    self.c.i64c(rm as i64);
                } else {
                    self.xr(rm);
                }
                self.c.set(T1);
                if !sf {
                    self.c.get(T1);
                    self.trunc32(false);
                    self.c.set(T1);
                }
                self.c.get(T0);
                self.c.get(T1);
                if sub {
                    self.c.i64_sub();
                } else {
                    self.c.i64_add();
                }
                self.c.tee(T3);
                self.trunc32(sf);
                self.c.set(T2);
                self.flags_addsub(T0, T1, T2, T3, sub, sf);
            }
            self.c.else_();
            {
                let l = *self.l;
                self.set_flag(l.n, |g| g.c.i32c((nzcv >> 3 & 1) as i32));
                self.set_flag(l.z, |g| g.c.i32c((nzcv >> 2 & 1) as i32));
                self.set_flag(l.c, |g| g.c.i32c((nzcv >> 1 & 1) as i32));
                self.set_flag(l.v, |g| g.c.i32c((nzcv & 1) as i32));
            }
            self.c.end();
            return true;
        }
        if insn & 0x1f00_0000 == 0x1b00_0000 {
            // MADD/MSUB only
            if field(insn, 29, 2) != 0 || field(insn, 21, 3) != 0 {
                return false;
            }
            let ra = field(insn, 10, 5);
            let o0 = bit(insn, 15);
            self.wr(rd, |g| {
                g.xr(ra);
                g.xr(rn);
                g.xr(rm);
                g.c.i64_mul();
                if o0 {
                    g.c.i64_sub();
                } else {
                    g.c.i64_add();
                }
                g.trunc32(sf);
            });
            return true;
        }
        if insn & 0x5fe0_0000 == 0x1ac0_0000 {
            // Variable shifts (LSLV/LSRV/ASRV/RORV); division stays interpreted
            if bit(insn, 29) {
                return false;
            }
            let kind = match field(insn, 10, 6) {
                0b001000 => 0,
                0b001001 => 1,
                0b001010 => 2,
                0b001011 => 3,
                _ => return false,
            };
            self.wr(rd, |g| {
                g.xr(rn);
                if !sf {
                    match kind {
                        2 => g.c.i64_extend32_s(),
                        0 => {},
                        _ => g.trunc32(false),
                    }
                }
                g.xr(rm);
                g.c.i64c(if sf { 63 } else { 31 });
                g.c.i64_and();
                if !sf && kind == 3 {
                    // 32-bit rotate: (x >> s) | (x << ((32 - s) & 31)), x zero-extended
                    g.c.set(T4);
                    g.c.set(T3);
                    g.c.get(T3);
                    g.c.get(T4);
                    g.c.i64_shr_u();
                    g.c.get(T3);
                    g.c.i64c(32);
                    g.c.get(T4);
                    g.c.i64_sub();
                    g.c.i64c(31);
                    g.c.i64_and();
                    g.c.i64_shl();
                    g.c.i64_or();
                } else {
                    match kind {
                        0 => g.c.i64_shl(),
                        1 => g.c.i64_shr_u(),
                        2 => g.c.i64_shr_s(),
                        _ => g.c.i64_rotr(),
                    }
                }
                g.trunc32(sf);
            });
            return true;
        }
        false
    }

    fn branch(&mut self, insn: u32) -> bool {
        let off = self.off;
        let k = self.k + 1;
        if insn & 0x7c00_0000 == 0x1400_0000 {
            // B / BL
            let target = off + (sext((insn & 0x3ff_ffff) as u64, 26) << 2);
            if bit(insn, 31) {
                self.wr(30, |g| g.pc_at(off + 4));
            }
            self.goto(target, k);
            return true;
        }
        if insn & 0x7e00_0000 == 0x3400_0000
            || insn & 0x7e00_0000 == 0x3600_0000
            || insn & 0xff00_0010 == 0x5400_0000
        {
            // CBZ/CBNZ, TBZ/TBNZ, B.cond: taken -> target, else fall through
            let (target, cond_fn): (i64, Box<dyn Fn(&mut Self)>) =
                if insn & 0x7e00_0000 == 0x3400_0000 {
                    let sf = bit(insn, 31);
                    let rt = insn & 31;
                    let nz = bit(insn, 24);
                    (
                        off + (sext(field(insn, 5, 19) as u64, 19) << 2),
                        Box::new(move |g: &mut Self| {
                            g.xr(rt);
                            g.trunc32(sf);
                            g.c.i64_eqz();
                            if nz {
                                g.c.i32_eqz();
                            }
                        }),
                    )
                } else if insn & 0x7e00_0000 == 0x3600_0000 {
                    let b = (field(insn, 31, 1) << 5) | field(insn, 19, 5);
                    let rt = insn & 31;
                    let nz = bit(insn, 24);
                    (
                        off + (sext(field(insn, 5, 14) as u64, 14) << 2),
                        Box::new(move |g: &mut Self| {
                            g.xr(rt);
                            g.c.i64c(b as i64);
                            g.c.i64_shr_u();
                            g.c.i64c(1);
                            g.c.i64_and();
                            g.c.i32_wrap_i64();
                            if !nz {
                                g.c.i32_eqz();
                            }
                        }),
                    )
                } else {
                    let cond = insn & 0xf;
                    (
                        off + (sext(field(insn, 5, 19) as u64, 19) << 2),
                        Box::new(move |g: &mut Self| g.cond(cond)),
                    )
                };
            cond_fn(self);
            self.c.if_(EMPTY);
            self.nest += 1;
            self.goto(target, k);
            self.nest -= 1;
            self.c.end();
            self.goto(off + 4, k);
            return true;
        }
        if insn & 0xfe00_0000 == 0xd600_0000 {
            // BR / BLR / RET (ERET and others are interpreted)
            let opc = field(insn, 21, 4);
            if field(insn, 16, 5) != 31 || field(insn, 10, 6) != 0 || insn & 31 != 0 || opc > 2 {
                return false;
            }
            let rn = field(insn, 5, 5);
            self.xr(rn);
            self.c.set(T0);
            if opc == 1 {
                self.wr(30, |g| g.pc_at(off + 4));
            }
            self.store_pc(|g| g.c.get(T0));
            self.exit(STATUS_DONE, k);
            return true;
        }
        if insn & 0xffff_f01f == 0xd503_201f {
            // Hints (NOP, YIELD, SEV, BTI, PAC* in the hint space) other than WFI/WFE
            let crm_op2 = field(insn, 5, 7);
            return crm_op2 != 2 && crm_op2 != 3;
        }
        false
    }

    // ------------------------------------------------- system instructions

    /// Barriers (no-ops on one core) and MRS/MSR of registers kept in the
    /// CPU struct. Everything else in the system space is interpreted.
    fn system(&mut self, insn: u32) -> bool {
        if insn & 0xffff_f01f == 0xd503_301f
            && matches!(field(insn, 5, 3), 4 | 5 | 6)
            && insn & 0xf00 != 0
        {
            // DSB/DMB (CRm != 0) and ISB. With several CPUs a DSB may have to
            // wait for TLB shootdowns: interpreted.
            return !(self.l.smp != 0 && field(insn, 5, 3) == 4);
        }
        if insn & 0xffff_f0ff == 0xd503_30df {
            return true; // ISB (any CRm)
        }
        let l = *self.l;
        let mrs = insn & 0xfff0_0000 == 0xd530_0000;
        let msr = insn & 0xfff0_0000 == 0xd510_0000;
        if !mrs && !msr {
            return false;
        }
        let key = field(insn, 5, 16);
        let rt = insn & 31;
        let el1 = l.el == 1;
        let (addr, wide) = match key {
            k if k == sr(3, 3, 13, 0, 2) => (l.tpidr_el0, true),
            k if k == sr(3, 3, 13, 0, 3) && (mrs || el1) => (l.tpidrro_el0, true),
            k if k == sr(3, 0, 13, 0, 4) && el1 => (l.tpidr_el1, true),
            k if k == sr(3, 0, 4, 1, 0) && el1 && l.spsel != 0 => (l.sp_el0, true),
            k if k == sr(3, 3, 4, 2, 1) && el1 && mrs => (l.daif, false),
            _ => return false,
        };
        if mrs {
            self.wr(rt, |g| {
                g.c.i32c(0);
                if wide {
                    g.c.i64_load(addr);
                } else {
                    g.c.i64_load32_u(addr);
                }
            });
        } else {
            self.c.i32c(0);
            self.xr(rt);
            self.c.i64_store(addr);
        }
        true
    }

    // ---------------------------------------------- exclusives and acquire/release

    /// Push the zero-extended `size`-byte value at host offset ST (relative to mem).
    fn load_at_st(&mut self, size: u32) {
        let l = *self.l;
        self.c.get(ST);
        match size {
            1 => self.c.i64_load8_u(l.mem),
            2 => self.c.i64_load16_u(l.mem),
            4 => self.c.i64_load32_u(l.mem),
            _ => self.c.i64_load(l.mem),
        }
    }

    /// Store the value pushed by `f` (low `size` bytes) at host offset ST.
    fn store_at_st(&mut self, size: u32, f: impl FnOnce(&mut Self)) {
        let l = *self.l;
        self.c.get(ST);
        f(self);
        match size {
            1 => self.c.i64_store8(l.mem),
            2 => self.c.i64_store16(l.mem),
            4 => self.c.i64_store32(l.mem),
            _ => self.c.i64_store(l.mem),
        }
    }

    /// LDXR/LDAXR/STXR/STLXR (single register) and LDAR/STLR. The fast TLB
    /// path is inline; anything else (a miss, a page-crossing access) runs the
    /// interpreter for the instruction, so faults and corner cases match it.
    fn ldst_exclusive(&mut self, insn: u32) -> bool {
        let size = 1u32 << field(insn, 30, 2);
        let o2 = bit(insn, 23);
        let load = bit(insn, 22);
        let o1 = bit(insn, 21);
        let rs = field(insn, 16, 5);
        let o0 = bit(insn, 15);
        let rt2 = field(insn, 10, 5);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        if o1 || rt2 != 31 {
            return false; // pairs, CAS, UNDEFINED encodings
        }
        if !o2 && self.l.smp != 0 {
            return false; // several CPUs: compare-and-swap in the interpreter
        }
        if o2 && (rs != 31 || !o0) {
            return false;
        }
        if !o2 && load && rs != 31 {
            return false;
        }
        if !o2 && !load && (rs == rt || (rs == rn && rn != 31)) {
            return false; // CONSTRAINED UNPREDICTABLE: leave to the interpreter
        }
        let l = *self.l;
        self.xs(rn);
        self.c.set(VA);
        if o2 {
            // LDAR/STLR: ordinary accesses on one core
            if load {
                self.load(size);
                self.c.set(T0);
                self.wr(rt, |g| g.c.get(T0));
            } else {
                self.xr(rt);
                self.c.set(DATA);
                self.store(size);
            }
            return true;
        }
        if load {
            self.tlb_probe(l.read_tag, size);
            self.c.if_(EMPTY);
            self.host_addr(l.read_base);
            self.c.set(ST);
            self.load_at_st(size);
            self.c.set(T0);
            self.wr(rt, |g| g.c.get(T0));
            self.c.i32c(0);
            self.c.get(ST);
            self.c.i64_extend_i32_u();
            self.c.i64_store(l.excl_addr);
            self.c.i32c(0);
            self.c.i32c(1);
            self.c.i32_store8(l.excl_armed);
            self.c.else_();
            self.interp(insn);
            self.c.end();
        } else {
            self.tlb_probe(l.write_tag, size);
            self.c.if_(EMPTY);
            self.host_addr(l.write_base);
            self.c.set(ST);
            // ok = armed && same 64-byte granule
            self.c.i32c(0);
            self.c.i32_load8_u(l.excl_armed);
            self.c.i32c(0);
            self.c.i64_load(l.excl_addr);
            self.c.get(ST);
            self.c.i64_extend_i32_u();
            self.c.i64_xor();
            self.c.i64c(!0x3f);
            self.c.i64_and();
            self.c.i64_eqz();
            self.c.i32_and();
            self.c.if_(EMPTY);
            self.store_at_st(size, |g| g.xr(rt));
            self.wr(rs, |g| g.c.i64c(0));
            self.c.else_();
            self.wr(rs, |g| g.c.i64c(1));
            self.c.end();
            self.c.i32c(0);
            self.c.i32c(0);
            self.c.i32_store8(l.excl_armed);
            self.c.else_();
            self.interp(insn);
            self.c.end();
        }
        true
    }

    // ------------------------------------------------ more data processing

    /// EXTR/ROR, CLZ, REV*, UDIV/SDIV, UMULH/SMULH, [SU]M{ADD,SUB}L.
    fn dp_extra(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31);
        let rd = insn & 31;
        let rn = field(insn, 5, 5);
        let rm = field(insn, 16, 5);
        if insn & 0x7fa0_0000 == 0x1380_0000 {
            // EXTR (N must equal sf; imms < 32 for 32-bit)
            let n = bit(insn, 22);
            let lsb = field(insn, 10, 6);
            if n != sf || (!sf && lsb >= 32) {
                return false;
            }
            let width = if sf { 64 } else { 32 };
            self.wr(rd, |g| {
                if lsb == 0 {
                    g.xr(rm);
                    g.trunc32(sf);
                } else {
                    g.xr(rm);
                    g.trunc32(sf);
                    g.c.i64c(lsb as i64);
                    g.c.i64_shr_u();
                    g.xr(rn);
                    g.c.i64c((width - lsb) as i64);
                    g.c.i64_shl();
                    g.c.i64_or();
                    g.trunc32(sf);
                }
            });
            return true;
        }
        if insn & 0x7fff_c000 == 0x5ac0_0000 {
            // Data-processing (1 source): RBIT/REV16/REV/REV32/CLZ/CLS
            let opc = field(insn, 10, 6);
            match (opc, sf) {
                (4, _) => {
                    // CLZ
                    self.wr(rd, |g| {
                        g.xr(rn);
                        if sf {
                            g.c.i64_clz();
                        } else {
                            g.c.i32_wrap_i64();
                            g.c.i32_clz();
                            g.c.i64_extend_i32_u();
                        }
                    });
                },
                (1, _) | (2, _) | (3, true) => {
                    // REV16 (1), REV32 / REV(32-bit) (2), REV (64-bit) (3)
                    self.xr(rn);
                    self.c.set(T0);
                    // swap bytes within 16-bit units
                    self.swap_units(T0, 8, 0x00ff_00ff_00ff_00ff);
                    if opc >= 2 {
                        self.swap_units(T0, 16, 0x0000_ffff_0000_ffff);
                    }
                    if opc == 3 {
                        self.c.get(T0);
                        self.c.i64c(32);
                        self.c.i64_rotr();
                        self.c.set(T0);
                    }
                    self.wr(rd, |g| {
                        g.c.get(T0);
                        g.trunc32(sf);
                    });
                },
                _ => return false,
            }
            return true;
        }
        if insn & 0x7fe0_f800 == 0x1ac0_0800 {
            // UDIV / SDIV (division by zero gives 0; INT_MIN / -1 wraps)
            let signed = bit(insn, 10);
            self.xr(rn);
            self.ext_operand(sf, signed);
            self.c.set(T0);
            self.xr(rm);
            self.ext_operand(sf, signed);
            self.c.set(T1);
            self.wr(rd, |g| {
                g.c.get(T1);
                g.c.i64_eqz();
                g.c.if_(I64);
                g.c.i64c(0);
                g.c.else_();
                if signed {
                    g.c.get(T1);
                    g.c.i64c(-1);
                    g.c.i64_eq();
                    g.c.if_(I64);
                    g.c.i64c(0);
                    g.c.get(T0);
                    g.c.i64_sub();
                    g.c.else_();
                    g.c.get(T0);
                    g.c.get(T1);
                    g.c.i64_div_s();
                    g.c.end();
                } else {
                    g.c.get(T0);
                    g.c.get(T1);
                    g.c.i64_div_u();
                }
                g.c.end();
                g.trunc32(sf);
            });
            return true;
        }
        if insn & 0xff60_8000 == 0x9b40_0000 && field(insn, 10, 5) == 31 {
            // UMULH (U=1) / SMULH
            let unsigned = bit(insn, 23);
            self.xr(rn);
            self.c.set(T0);
            self.xr(rm);
            self.c.set(T1);
            self.umulh(T0, T1);
            if !unsigned {
                // signed high = unsigned high - (a < 0 ? b : 0) - (b < 0 ? a : 0)
                self.c.get(T1);
                self.c.i64c(0);
                self.c.get(T0);
                self.c.i64c(0);
                self.c.i64_lt_s();
                self.c.select();
                self.c.i64_sub();
                self.c.get(T0);
                self.c.i64c(0);
                self.c.get(T1);
                self.c.i64c(0);
                self.c.i64_lt_s();
                self.c.select();
                self.c.i64_sub();
            }
            self.c.set(T2);
            self.wr(rd, |g| g.c.get(T2));
            return true;
        }
        if insn & 0xff60_0000 == 0x9b20_0000 {
            // SMADDL/SMSUBL (U=0), UMADDL/UMSUBL (U=1): Xa +/- Wn * Wm
            let unsigned = bit(insn, 23);
            let sub = bit(insn, 15);
            let ra = field(insn, 10, 5);
            self.wr(rd, |g| {
                g.xr(ra);
                g.xr(rn);
                g.ext_operand(false, !unsigned);
                g.xr(rm);
                g.ext_operand(false, !unsigned);
                g.c.i64_mul();
                if sub {
                    g.c.i64_sub();
                } else {
                    g.c.i64_add();
                }
            });
            return true;
        }
        false
    }

    /// For 32-bit operations, reduce the i64 on the stack to its low 32 bits,
    /// sign- or zero-extended.
    fn ext_operand(&mut self, sf: bool, signed: bool) {
        if !sf {
            if signed {
                self.c.i64_extend32_s();
            } else {
                self.c.i64c(0xffff_ffff);
                self.c.i64_and();
            }
        }
    }

    /// local t = ((t >> n) & mask) | ((t & mask) << n)
    fn swap_units(&mut self, t: u32, n: i64, mask: i64) {
        self.c.get(t);
        self.c.i64c(n);
        self.c.i64_shr_u();
        self.c.i64c(mask);
        self.c.i64_and();
        self.c.get(t);
        self.c.i64c(mask);
        self.c.i64_and();
        self.c.i64c(n);
        self.c.i64_shl();
        self.c.i64_or();
        self.c.set(t);
    }

    /// Push the high 64 bits of the unsigned product of locals a and b.
    /// Uses T2..T4 and DATA as scratch.
    fn umulh(&mut self, a: u32, b: u32) {
        let lo = |g: &mut Self, x: u32| {
            g.c.get(x);
            g.c.i64c(0xffff_ffff);
            g.c.i64_and();
        };
        let hi = |g: &mut Self, x: u32| {
            g.c.get(x);
            g.c.i64c(32);
            g.c.i64_shr_u();
        };
        // T2 = lo(a)*hi(b), T3 = hi(a)*lo(b)
        lo(self, a);
        hi(self, b);
        self.c.i64_mul();
        self.c.set(T2);
        hi(self, a);
        lo(self, b);
        self.c.i64_mul();
        self.c.set(T3);
        // T4 = (lo(a)*lo(b) >> 32) + lo(T2) + lo(T3)   (the middle column)
        lo(self, a);
        lo(self, b);
        self.c.i64_mul();
        self.c.i64c(32);
        self.c.i64_shr_u();
        lo(self, T2);
        self.c.i64_add();
        lo(self, T3);
        self.c.i64_add();
        self.c.set(T4);
        // hi(a)*hi(b) + hi(T2) + hi(T3) + hi(T4)
        hi(self, a);
        hi(self, b);
        self.c.i64_mul();
        hi(self, T2);
        self.c.i64_add();
        hi(self, T3);
        self.c.i64_add();
        hi(self, T4);
        self.c.i64_add();
    }

    fn ldst(&mut self, insn: u32) -> bool {
        if bit(insn, 26) {
            return false; // SIMD&FP
        }
        if insn & 0x3f00_0000 == 0x0800_0000 {
            return self.ldst_exclusive(insn);
        }
        if insn & 0x3800_0000 == 0x2800_0000 {
            return self.ldst_pair(insn);
        }
        if insn & 0x3800_0000 == 0x3800_0000 {
            return self.ldst_reg(insn);
        }
        false
    }

    fn ldst_pair(&mut self, insn: u32) -> bool {
        let opc = field(insn, 30, 2);
        let mode = field(insn, 23, 2);
        let load = bit(insn, 22);
        let (size, signed) = match (opc, load) {
            (0, _) => (4u32, false),
            (2, _) => (8, false),
            (1, true) if mode != 0 => (4, true),
            _ => return false,
        };
        let rt2 = field(insn, 10, 5);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let imm = sext(field(insn, 15, 7) as u64, 7) * size as i64;
        // base in T1, address in VA
        self.xs(rn);
        self.c.set(T1);
        self.c.get(T1);
        if mode != 1 {
            self.c.i64c(imm);
            self.c.i64_add();
        }
        self.c.set(VA);
        if load {
            self.load(size);
            self.c.set(T2);
            self.c.get(VA);
            self.c.i64c(size as i64);
            self.c.i64_add();
            self.c.set(VA);
            self.load(size);
            self.c.set(T3);
            let ext = |g: &mut Self, l: u32| {
                g.c.get(l);
                if signed {
                    g.c.i64_extend32_s();
                }
            };
            self.wr(rt, |g| ext(g, T2));
            self.wr(rt2, |g| ext(g, T3));
        } else {
            self.xr(rt);
            self.c.set(DATA);
            self.xr(rt2);
            self.c.set(T2);
            self.store(size);
            self.c.get(T2);
            self.c.set(DATA);
            self.c.get(VA);
            self.c.i64c(size as i64);
            self.c.i64_add();
            self.c.set(VA);
            self.store(size);
        }
        if mode == 1 || mode == 3 {
            self.ws(rn, |g| {
                g.c.get(T1);
                g.c.i64c(imm);
                g.c.i64_add();
            });
        }
        true
    }

    fn ldst_reg(&mut self, insn: u32) -> bool {
        let size_log = field(insn, 30, 2);
        let opc = field(insn, 22, 2);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let size = 1u32 << size_log;
        // Address into VA; writeback amount (if any) in `wb`
        let mut wb: Option<(bool, i64)> = None; // (post-index, imm)
        if bit(insn, 24) {
            let imm = (field(insn, 10, 12) as i64) << size_log;
            self.xs(rn);
            if imm != 0 {
                self.c.i64c(imm);
                self.c.i64_add();
            }
            self.c.set(VA);
        } else if !bit(insn, 21) {
            let imm = sext(field(insn, 12, 9) as u64, 9);
            match field(insn, 10, 2) {
                0 => {
                    self.xs(rn);
                    self.c.i64c(imm);
                    self.c.i64_add();
                    self.c.set(VA);
                },
                1 => {
                    self.xs(rn);
                    self.c.set(VA);
                    wb = Some((true, imm));
                },
                3 => {
                    self.xs(rn);
                    self.c.i64c(imm);
                    self.c.i64_add();
                    self.c.set(VA);
                    wb = Some((false, imm));
                },
                _ => return false, // unprivileged
            }
        } else if field(insn, 10, 2) == 2 {
            let rm = field(insn, 16, 5);
            let option = field(insn, 13, 3);
            if option & 2 == 0 {
                return false;
            }
            let shift = if bit(insn, 12) { size_log } else { 0 };
            self.xs(rn);
            self.xr(rm);
            match option {
                2 => self.trunc32(false),
                6 => self.c.i64_extend32_s(),
                _ => {},
            }
            if shift != 0 {
                self.c.i64c(shift as i64);
                self.c.i64_shl();
            }
            self.c.i64_add();
            self.c.set(VA);
        } else {
            return false;
        }
        match opc {
            0 => {
                self.xr(rt);
                self.c.set(DATA);
                self.store(size);
            },
            1 => {
                self.load(size);
                self.c.set(T2);
                self.wr(rt, |g| g.c.get(T2));
            },
            2 if size_log == 3 => {
                // PRFM: no writeback forms reach here (interpreted as UNDEFINED)
                if wb.is_some() {
                    return false;
                }
            },
            2 | 3 => {
                if opc == 3 && size_log >= 2 {
                    return false;
                }
                self.load(size);
                match size {
                    1 => self.c.i64_extend8_s(),
                    2 => self.c.i64_extend16_s(),
                    _ => self.c.i64_extend32_s(),
                }
                if opc == 3 {
                    self.trunc32(false);
                }
                self.c.set(T2);
                self.wr(rt, |g| g.c.get(T2));
            },
            _ => return false,
        }
        if let Some((post, imm)) = wb {
            self.ws(rn, |g| {
                g.c.get(VA);
                if post {
                    g.c.i64c(imm);
                    g.c.i64_add();
                }
            });
        }
        true
    }
}

/// The instructions of the block starting at word `start` of a page.
pub fn block_insns(words: &[u32], start: usize, max: usize) -> &[u32] {
    let mut end = start;
    while end < words.len() && end - start < max {
        let insn = words[end];
        end += 1;
        if ends_block(insn) && !is_hint(insn) {
            break;
        }
    }
    &words[start..end]
}

/// Translate the blocks at `entries` (sorted, distinct byte offsets) of the
/// page whose instruction words are `words` into a wasm module.
pub fn translate_page(
    layout: &Layout,
    words: &[u32],
    entries: &[u16],
    max_block: usize,
) -> Vec<u8> {
    let n_entries = entries.len() as u32;
    let mut g = Gen {
        l: layout,
        c: Code::new(),
        off: 0,
        k: 0,
        entries,
        entry_off: 0,
        depth_top: 0,
        nest: 0,
    };
    // loop $top { block $miss { block $e[N-1] ... { block $e0 { br_table } e0 } ... } miss }
    g.c.loop_(EMPTY);
    g.c.block(EMPTY);
    for _ in 0..n_entries {
        g.c.block(EMPTY);
    }
    let first = entries[0] as u32 / 4;
    let last = entries[entries.len() - 1] as u32 / 4;
    let mut targets = vec![n_entries; (last - first + 1) as usize];
    for (i, &e) in entries.iter().enumerate() {
        targets[(e as u32 / 4 - first) as usize] = i as u32;
    }
    g.c.get(L_PC);
    g.c.i32_wrap_i64();
    g.c.i32c(0xfff);
    g.c.i32_and();
    g.c.i32c(2);
    g.c.i32_shr_u();
    if first != 0 {
        g.c.i32c(first as i32);
        g.c.i32_sub();
    }
    g.c.br_table(&targets, n_entries);
    for (i, &e) in entries.iter().enumerate() {
        g.c.end(); // block $e_i: entry i's code follows
        g.entry_off = e as i64;
        g.depth_top = n_entries - i as u32;
        g.nest = 0;
        let insns = block_insns(words, e as usize / 4, max_block);
        translate_block(&mut g, insns);
    }
    g.c.end(); // $miss: the PC is not an entry of this module
    g.store_pc(|g| g.c.get(L_PC));
    g.c.i32c(STATUS_DONE);
    g.c.ret();
    g.c.end(); // $top
    g.c.i32c(STATUS_DONE);
    wasm::module(&imports(), LOCALS, &g.c)
}

fn translate_block(g: &mut Gen, insns: &[u32]) {
    let mut n = 0;
    for (k, &insn) in insns.iter().enumerate() {
        g.k = k as u32;
        g.off = 4 * k as i64;
        n = k as u32 + 1;
        // Branches end the block inside `insn()`; system instructions and
        // exception generation go to the interpreter and also end it.
        let start = g.c.b.len();
        if g.insn(insn) {
            if ends_block(insn) && !is_hint(insn) {
                return;
            }
        } else {
            g.c.b.truncate(start);
            g.interp(insn);
            if ends_block(insn) && !is_hint(insn) {
                // Interpreted control flow fell through: continue after it
                let off = g.off + 4;
                g.goto(off, n);
                return;
            }
        }
    }
    let off = 4 * n as i64;
    g.goto(off, n);
}

fn is_hint(insn: u32) -> bool {
    insn & 0xffff_f01f == 0xd503_201f && !matches!(field(insn, 5, 7), 2 | 3)
}
