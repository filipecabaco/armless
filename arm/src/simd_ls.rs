//! SIMD&FP loads and stores: LDR/STR (B/H/S/D/Q), LDP/STP, LDR literal, and
//! the AdvSIMD structure loads/stores LD1-4/ST1-4 (multiple, single lane,
//! replicate).

use crate::bits::{bit, field, sext};
use crate::board::Host;
use crate::cpu::{Cpu, Event, Res};

impl<H: Host> Cpu<H> {
    /// Read `bytes` (1..16) little-endian bytes at `va` as a u128.
    pub(crate) fn read_vec(&mut self, va: u64, bytes: u32) -> Res<u128> {
        if bytes <= 8 {
            Ok(self.read(va, bytes, false)? as u128)
        } else {
            let lo = self.read(va, 8, false)? as u128;
            let hi = self.read(va.wrapping_add(8), 8, false)? as u128;
            Ok(lo | hi << 64)
        }
    }

    pub(crate) fn write_vec(&mut self, va: u64, bytes: u32, v: u128) -> Res<()> {
        if bytes <= 8 {
            self.write(va, bytes, v as u64, false)
        } else {
            // Check both halves first so a fault leaves memory untouched.
            self.translate(va, crate::mmu::Access::Write, false)?;
            self.translate(va.wrapping_add(15), crate::mmu::Access::Write, false)?;
            self.write(va, 8, v as u64, false)?;
            self.write(va.wrapping_add(8), 8, (v >> 64) as u64, false)
        }
    }

    /// Dispatch for loads/stores with the V bit set.
    pub(crate) fn simd_ldst(&mut self, insn: u32) -> Res<()> {
        self.check_fp_access()?;
        if insn & 0xbfbf_0000 == 0x0c00_0000 || insn & 0xbfa0_0000 == 0x0c80_0000 {
            return self.ldst_multiple(insn);
        }
        if insn & 0xbf9f_0000 == 0x0d00_0000 || insn & 0xbf80_0000 == 0x0d80_0000 {
            return self.ldst_single(insn);
        }
        if insn & 0x3b00_0000 == 0x1800_0000 {
            // LDR (literal, SIMD&FP)
            let bytes = match field(insn, 30, 2) {
                0 => 4,
                1 => 8,
                2 => 16,
                _ => return Err(Event::undefined()),
            };
            let off = sext(field(insn, 5, 19) as u64, 19) << 2;
            let v = self.read_vec(self.pc.wrapping_add(off as u64), bytes)?;
            self.vreg[(insn & 31) as usize] = v;
            return Ok(());
        }
        if insn & 0x3800_0000 == 0x2800_0000 {
            return self.ldst_pair_simd(insn);
        }
        if insn & 0x3800_0000 == 0x3800_0000 {
            return self.ldst_reg_simd(insn);
        }
        Err(Event::undefined())
    }

    fn ldst_pair_simd(&mut self, insn: u32) -> Res<()> {
        let bytes: u32 = match field(insn, 30, 2) {
            0 => 4,
            1 => 8,
            2 => 16,
            _ => return Err(Event::undefined()),
        };
        let mode = field(insn, 23, 2);
        let load = bit(insn, 22);
        let rt2 = field(insn, 10, 5) as usize;
        let rn = field(insn, 5, 5);
        let rt = (insn & 31) as usize;
        let imm = sext(field(insn, 15, 7) as u64, 7) * bytes as i64;
        let base = self.xs(rn);
        let addr = if mode == 1 { base } else { base.wrapping_add(imm as u64) };
        if load {
            let a = self.read_vec(addr, bytes)?;
            let b = self.read_vec(addr.wrapping_add(bytes as u64), bytes)?;
            self.vreg[rt] = a;
            self.vreg[rt2] = b;
        } else {
            let (a, b) = (self.vreg[rt], self.vreg[rt2]);
            self.write_vec(addr, bytes, a)?;
            self.write_vec(addr.wrapping_add(bytes as u64), bytes, b)?;
        }
        if mode == 1 || mode == 3 {
            self.ws(rn, base.wrapping_add(imm as u64));
        }
        Ok(())
    }

    fn ldst_reg_simd(&mut self, insn: u32) -> Res<()> {
        let size = field(insn, 30, 2);
        let opc = field(insn, 22, 2);
        // size:opc<1> selects the width; opc<0> is "load".
        let scale = if opc & 2 != 0 {
            if size != 0 {
                return Err(Event::undefined());
            }
            4
        } else {
            size
        };
        let load = opc & 1 != 0;
        let bytes = 1u32 << scale;
        let rn = field(insn, 5, 5);
        let rt = (insn & 31) as usize;
        let base = self.xs(rn);
        let mut writeback = None;
        let addr = if bit(insn, 24) {
            base.wrapping_add((field(insn, 10, 12) as u64) << scale)
        } else if !bit(insn, 21) {
            let imm = sext(field(insn, 12, 9) as u64, 9) as u64;
            match field(insn, 10, 2) {
                0 => base.wrapping_add(imm),
                1 => {
                    writeback = Some(base.wrapping_add(imm));
                    base
                },
                3 => {
                    let a = base.wrapping_add(imm);
                    writeback = Some(a);
                    a
                },
                _ => return Err(Event::undefined()), // no unprivileged SIMD forms
            }
        } else if field(insn, 10, 2) == 2 {
            let rm = field(insn, 16, 5);
            let option = field(insn, 13, 3);
            if option & 2 == 0 {
                return Err(Event::undefined());
            }
            let shift = if bit(insn, 12) { scale } else { 0 };
            let m = self.xr(rm);
            let ext = match option {
                2 => m & 0xffff_ffff,
                6 => m as u32 as i32 as i64 as u64,
                _ => m,
            };
            base.wrapping_add(ext << shift)
        } else {
            return Err(Event::undefined());
        };
        if load {
            let v = self.read_vec(addr, bytes)?;
            self.vreg[rt] = v;
        } else {
            let v = self.vreg[rt];
            self.write_vec(addr, bytes, v)?;
        }
        if let Some(wb) = writeback {
            self.ws(rn, wb);
        }
        Ok(())
    }

    /// LD1/LD2/LD3/LD4, ST1/ST2/ST3/ST4 (multiple structures).
    fn ldst_multiple(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let load = bit(insn, 22);
        let post = bit(insn, 23);
        let rm = field(insn, 16, 5);
        let opcode = field(insn, 12, 4);
        let size = field(insn, 10, 2);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let (rpt, selem) = match opcode {
            0b0000 => (1, 4),
            0b0010 => (4, 1),
            0b0100 => (1, 3),
            0b0110 => (3, 1),
            0b0111 => (1, 1),
            0b1000 => (1, 2),
            0b1010 => (2, 1),
            _ => return Err(Event::undefined()),
        };
        if size == 3 && !q && selem != 1 {
            return Err(Event::undefined());
        }
        let ebytes = 1u64 << size;
        let elements = if q { 16 } else { 8 } / ebytes;
        let base = self.xs(rn);
        let mut addr = base;
        for r in 0..rpt {
            for e in 0..elements {
                for s in 0..selem {
                    let t = ((rt + r + s) % 32) as usize;
                    let shift = (e * ebytes * 8) as u32;
                    if load {
                        let v = self.read(addr, ebytes as u32, false)? as u128;
                        let mask = crate::bits::ones(ebytes as u32 * 8) as u128;
                        let mut reg = self.vreg[t];
                        if e == 0 && !q {
                            reg &= u64::MAX as u128; // upper half becomes zero below
                        }
                        reg = (reg & !(mask << shift)) | (v << shift);
                        if !q {
                            reg &= u64::MAX as u128;
                        }
                        self.vreg[t] = reg;
                    } else {
                        let v = (self.vreg[t] >> shift) as u64;
                        self.write(addr, ebytes as u32, v, false)?;
                    }
                    addr = addr.wrapping_add(ebytes);
                }
            }
        }
        if post {
            let offset = if rm == 31 { addr.wrapping_sub(base) } else { self.xr(rm) };
            self.ws(rn, base.wrapping_add(offset));
        }
        Ok(())
    }

    /// LD1-4/ST1-4 (single structure) and LD1R-LD4R (replicate).
    fn ldst_single(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let load = bit(insn, 22);
        let r = field(insn, 21, 1);
        let post = bit(insn, 23);
        let rm = field(insn, 16, 5);
        let opcode = field(insn, 13, 3);
        let s = field(insn, 12, 1);
        let size = field(insn, 10, 2);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let selem = ((opcode & 1) << 1 | r) + 1;
        let mut scale = opcode >> 1;
        let mut replicate = false;
        let index: u32;
        match scale {
            0 => index = (q as u32) << 3 | s << 2 | size,
            1 => {
                if size & 1 != 0 {
                    return Err(Event::undefined());
                }
                index = (q as u32) << 2 | s << 1 | (size >> 1);
            },
            2 => {
                if size & 2 != 0 {
                    return Err(Event::undefined());
                }
                if size & 1 == 0 {
                    index = (q as u32) << 1 | s;
                } else {
                    if s != 0 {
                        return Err(Event::undefined());
                    }
                    index = q as u32;
                    scale = 3;
                }
            },
            _ => {
                if !load || s != 0 {
                    return Err(Event::undefined());
                }
                scale = size;
                replicate = true;
                index = 0;
            },
        }
        let ebytes = 1u64 << scale;
        let ebits = (ebytes * 8) as u32;
        let mask = crate::bits::ones(ebits) as u128;
        let base = self.xs(rn);
        let mut addr = base;
        for k in 0..selem {
            let t = ((rt + k) % 32) as usize;
            if replicate {
                let v = self.read(addr, ebytes as u32, false)? as u128;
                let mut reg = 0u128;
                let lanes = if q { 16 } else { 8 } / ebytes;
                for e in 0..lanes {
                    reg |= v << (e * ebytes * 8);
                }
                self.vreg[t] = reg;
            } else {
                let shift = index * ebits;
                if load {
                    let v = self.read(addr, ebytes as u32, false)? as u128;
                    self.vreg[t] = (self.vreg[t] & !(mask << shift)) | (v << shift);
                } else {
                    let v = (self.vreg[t] >> shift) as u64;
                    self.write(addr, ebytes as u32, v, false)?;
                }
            }
            addr = addr.wrapping_add(ebytes);
        }
        if post {
            let offset = if rm == 31 { addr.wrapping_sub(base) } else { self.xr(rm) };
            self.ws(rn, base.wrapping_add(offset));
        }
        Ok(())
    }
}
