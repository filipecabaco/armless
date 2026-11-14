use super::*;

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

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A register value: often an address in the window the data lives in, else the edge cases and noise.
    fn reg(&mut self) -> u64 {
        match self.next() & 7 {
            0 => 0,
            1 => u64::MAX,
            2..=4 => 0x40000 + (self.next() & 0x7ff),
            _ => self.next(),
        }
    }

    /// A register of the few that the programs of `flag_program` use, or SP.
    fn small_reg(&mut self) -> u32 {
        match self.next() % 10 {
            0 => 31,
            v => (v % 8) as u32,
        }
    }

    /// A short program of what sets the flags (SUBS and CMP, ADDS, ANDS and TST, CCMP, ADCS, FCMP, MSR NZCV)
    /// and of what reads them (B.cond, the selects, CCMP, ADC, MRS NZCV), with the branches forward only.
    fn flag_program(&mut self, n: usize) -> Vec<u32> {
        let mut code: Vec<u32> = Vec::new();
        while code.len() < n {
            let room = (n - 1 - code.len()) as u64;
            let (rd, rn, rm) = (self.small_reg(), self.small_reg(), self.small_reg());
            let cond = (self.next() % 16) as u32;
            // Clears the size bit of an encoding of 64 bits, for the 32-bit one.
            let sf = if self.next() & 1 != 0 { !0x8000_0000u32 } else { u32::MAX };
            let imm6 = (self.next() % 32) as u32;
            let imm12 = (self.next() % 0x1000) as u32;
            let sh = (self.next() % 3) as u32;
            let other = (self.next() % 16) as u32;
            let pick = (self.next() % 4) as usize;
            // A branch 1 to 6 instructions on, at most to the last one.
            let off = (1 + self.next() % 6).min(room) as u32;
            let bcond = if room == 0 { 0xd503_201f } else { 0x5400_0000 | off << 5 | cond };
            let imm_form = (0xf100_0000u32 & sf) | (sh & 1) << 22 | imm12 << 10 | rn << 5;
            let reg_form = (0xeb00_0000u32 & sf) | sh << 22 | rm << 16 | imm6 << 10 | rn << 5;
            match self.next() % 32 {
                // A compare and the branch on it.
                0..=9 => {
                    code.push(if pick & 1 != 0 { imm_form | 31 } else { reg_form | 31 });
                    code.push(bcond);
                },
                // SUBS, ADDS with an immediate or a register (shifted).
                10..=13 => code.push(match pick {
                    0 => imm_form | rd,
                    1 => reg_form | rd,
                    2 => (imm_form | rd) ^ 0x4000_0000,
                    _ => (reg_form | rd) ^ 0x4000_0000,
                }),
                // ANDS (and TST), the register shifted and maybe inverted.
                14..=15 => code.push(
                    (0xea00_0000u32 & sf)
                        | (other & 3) << 22
                        | (other >> 2 & 1) << 21
                        | rm << 16
                        | imm6 << 10
                        | rn << 5
                        | if other >> 3 != 0 { 31 } else { rd },
                ),
                16..=19 => code.push(bcond),
                // CSEL, CSINC, CSINV, CSNEG.
                20..=22 => code.push(
                    ([0x9a80_0000u32, 0x9a80_0400, 0xda80_0000, 0xda80_0400][pick] & sf)
                        | rm << 16
                        | cond << 12
                        | rn << 5
                        | rd,
                ),
                // CCMP and CCMN of a register or of an immediate (5 bits, in the place of the register).
                23..=25 => code.push(
                    ([0xfa40_0000u32, 0xba40_0000][pick & 1] & sf)
                        | if pick & 2 != 0 {
                            0x800 | (other | (16 * (imm6 & 1))) << 16
                        } else {
                            rm << 16
                        }
                        | cond << 12
                        | rn << 5
                        | other,
                ),
                // ADC, ADCS, SBC, SBCS.
                26..=27 => code.push(
                    ([0x9a00_0000u32, 0xba00_0000, 0xda00_0000, 0xfa00_0000][pick] & sf)
                        | rm << 16
                        | rn << 5
                        | rd,
                ),
                // MRS and MSR of NZCV, which `exec_slow` runs.
                28 => code.push(0xd53b_4200 | rd),
                29 => code.push(0xd51b_4200 | (rd & 7)),
                // FCMP, which writes the flags in `exec_slow` too.
                30 => code.push(0x1e60_2000 | (rm & 7) << 16 | (rn & 7) << 5),
                // ADD of an immediate, with SP for either register.
                _ => code.push(0x9100_0000 | imm6 << 10 | rn << 5 | rd),
            }
        }
        code.truncate(n);
        code
    }

    /// An instruction word of the classes the loop covers, but not the system ones (SVC, MSR and the like)
    /// other than the hints and barriers.
    fn insn(&mut self) -> u32 {
        loop {
            let insn = self.next() as u32;
            let (crm, op2) = ((insn >> 8) & 15, (insn >> 5) & 7);
            match insn >> 27 {
                // A hint (not WFI, which has the CPU wait) or a barrier.
                0 if (crm, op2) != (0, 3) => return 0xd503_201f | crm << 8 | op2 << 5,
                1 if matches!(op2, 2 | 4 | 5 | 6) => {
                    return 0xd503_3000 | crm << 8 | op2 << 5 | 0x1f
                },
                _ => {},
            }
            if insn >> 25 != 0b1101010 {
                return insn;
            }
        }
    }
}

const DATA: std::ops::Range<usize> = 0x3f000..0x42000;

/// The loop and `execute` (the decoder `exec_slow` reaches) leave the same registers, flags, memory, pc
/// and exception state after one random instruction.
#[test]
fn a_pair_of_simd_stores_is_stored_whole_or_not_at_all() {
    for scale in [2u32, 3, 4] {
        let size = 1usize << scale;
        let mut c = Cpu::new(4 << 20, NoDevices);
        let v1 = 0x1111_2222_3333_4444_5555_6666_7777_8888u128;
        let v2 = !v1;
        // Page 0x40 is served for writes, page 0x41 is not.
        let el = c.el as usize;
        c.fast[el].write_tag[0x40] = 0x40;
        c.fast[el].write_base[0x40] = 0x40000;
        c.mem.write(0x40000, &[0xaa; 0x1000]);
        c.mem.write(0x41000, &[0xaa; 0x1000]);
        // The first component is in page 0x40, the second is not: nothing is stored.
        let va = 0x41000 - size as u64;
        assert!(!c.vwrite_pair(va, scale, v1, v2));
        assert!(c.mem.to_vec()[0x41000 - size..0x42000].iter().all(|&b| b == 0xaa));
        // Both are in page 0x40: both are stored, the low half of each first.
        assert!(c.vwrite_pair(0x40010, scale, v1, v2));
        let got = |c: &Cpu<NoDevices>, a: usize| {
            let mut b = [0u8; 16];
            c.mem.read(a, &mut b[..size]);
            u128::from_le_bytes(b)
        };
        let mask = if scale == 4 { u128::MAX } else { (1u128 << (8 * size)) - 1 };
        assert_eq!(got(&c, 0x10 + 0x40000), v1 & mask);
        assert_eq!(got(&c, 0x10 + 0x40000 + size), v2 & mask);
    }
}

#[test]
fn the_loop_agrees_with_execute_on_random_instructions() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut bad, mut seen) = (Vec::new(), Vec::new());
    // The data pages only: a copy of all of RAM after each of the 400 000 instructions took minutes
    let data = |c: &Cpu<NoDevices>| {
        let mut v = vec![0; DATA.len()];
        c.mem.read(DATA.start, &mut v);
        v
    };
    for _ in 0..400 {
        let mut a = Cpu::new(4 << 20, NoDevices);
        let mut b = Cpu::new(4 << 20, NoDevices);
        // SIMD&FP on in most of them, and off in the others (the access traps).
        let fpen = if rng.next() & 3 != 0 { 3 << 20 } else { 0 };
        a.sys.cpacr = fpen;
        b.sys.cpacr = fpen;
        for i in DATA {
            let v = rng.next() as u8;
            a.mem.write(i, &[v]);
            b.mem.write(i, &[v]);
        }
        for j in 0..1000usize {
            let insn = rng.insn();
            let pc = 0x1000 + 4 * j as u64;
            for c in [&mut a, &mut b] {
                c.mem.write(pc as usize, &insn.to_le_bytes());
            }
            // The word was written behind the cache's back: the block decoded from the instruction
            // before this one has the old (zero) word in its place.
            a.jit.pre.drop_page((pc >> 12) as usize);
            for r in 0..31 {
                let v = rng.reg();
                a.x[r] = v;
                b.x[r] = v;
            }
            let (s0, s1, f) = (rng.reg(), rng.reg(), rng.next());
            for c in [&mut a, &mut b] {
                c.sp_el0 = s0;
                c.sp_el1 = s1;
                c.n = f & 1 != 0;
                c.z = f & 2 != 0;
                c.c = f & 4 != 0;
                c.v = f & 8 != 0;
                c.pc = pc;
            }
            a.run(1);
            b.next_pc = pc + 4;
            match b.execute(insn).map(|()| b.next_pc) {
                Ok(next) => b.pc = next,
                Err(ev) => {
                    let _ = b.step_event(pc, ev);
                },
            }
            let why = format!("insn {insn:#010x} at {pc:#x}");
            let diff = if a.x[..32] != b.x[..32] {
                let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
                Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
            } else if (a.sp_el0, a.sp_el1) != (b.sp_el0, b.sp_el1) {
                Some("sp".to_string())
            } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
                Some("flags".to_string())
            } else if a.vreg != b.vreg {
                Some("vreg".to_string())
            } else if a.pc != b.pc || a.el != b.el {
                Some(format!("pc {:#x} against {:#x}", a.pc, b.pc))
            } else if (a.sys.esr, a.sys.elr, a.sys.far) != (b.sys.esr, b.sys.elr, b.sys.far) {
                Some(format!(
                    "esr/elr/far {:#x} {:#x} {:#x} against {:#x} {:#x} {:#x}",
                    a.sys.esr, a.sys.elr, a.sys.far, b.sys.esr, b.sys.elr, b.sys.far
                ))
            } else if data(&a) != data(&b) {
                Some("memory".to_string())
            } else {
                None
            };
            if let Some(d) = diff {
                // One report per class of instruction, then a fresh pair of machines.
                if !seen.contains(&(insn >> 21)) && bad.len() < 40 {
                    seen.push(insn >> 21);
                    bad.push(format!("{why}: {d}"));
                }
                break;
            }
            assert_eq!(a.x[31], 0, "{why}");
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
/// Programs of compares, branches and flag readers run by the loop, in rounds (the page is decoded, and
/// its compares fused with their branches, by the first one), with a budget that may end between a
/// compare and its branch, leave what `execute` leaves, one instruction at a time.
#[test]
fn the_loop_agrees_with_execute_on_flag_sequences() {
    const N: usize = 32;
    const EDGES: [u64; 8] = [
        0,
        1,
        2,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x7fff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
    ];
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut bad = Vec::new();
    for prog in 0..300 {
        let code = rng.flag_program(N);
        let mut a = Cpu::new(4 << 20, NoDevices);
        let mut b = Cpu::new(4 << 20, NoDevices);
        a.sys.cpacr = 3 << 20;
        b.sys.cpacr = 3 << 20;
        for (j, w) in code.iter().enumerate() {
            for c in [&mut a, &mut b] {
                c.mem.write(0x1000 + 4 * j, &w.to_le_bytes());
            }
        }
        for round in 0..4 {
            let budget = if round == 0 { N as u64 } else { 1 + rng.next() % N as u64 };
            for r in 0..31 {
                let v = if r < 8 { EDGES[(rng.next() % 8) as usize] } else { rng.reg() };
                a.x[r] = v;
                b.x[r] = v;
            }
            let (s0, s1, f) = (rng.reg(), rng.reg(), rng.next());
            for i in 0..8 {
                let v = (EDGES[(rng.next() % 8) as usize] as u128) << 64 | rng.next() as u128;
                a.vreg[i] = v;
                b.vreg[i] = v;
            }
            for c in [&mut a, &mut b] {
                c.sp_el0 = s0;
                c.sp_el1 = s1;
                c.n = f & 1 != 0;
                c.z = f & 2 != 0;
                c.c = f & 4 != 0;
                c.v = f & 8 != 0;
                c.pc = 0x1000;
            }
            a.run(budget);
            for _ in 0..budget {
                let pc = b.pc;
                let insn =
                    b.mem.load(pc as usize, 4) as u32;
                b.next_pc = pc + 4;
                match b.execute(insn).map(|()| b.next_pc) {
                    Ok(next) => b.pc = next,
                    Err(ev) => {
                        let _ = b.step_event(pc, ev);
                    },
                }
            }
            let diff = if a.x[..32] != b.x[..32] {
                let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
                Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
            } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
                Some(format!(
                    "flags {}{}{}{} against {}{}{}{}",
                    a.n as u8,
                    a.z as u8,
                    a.c as u8,
                    a.v as u8,
                    b.n as u8,
                    b.z as u8,
                    b.c as u8,
                    b.v as u8
                ))
            } else if a.pc != b.pc || a.el != b.el {
                Some(format!("pc {:#x} against {:#x}", a.pc, b.pc))
            } else if (a.sp_el0, a.sp_el1) != (b.sp_el0, b.sp_el1) {
                Some("sp".to_string())
            } else {
                None
            };
            if let Some(d) = diff {
                if bad.len() < 20 {
                    let words: Vec<String> = code.iter().map(|w| format!("{w:08x}")).collect();
                    bad.push(format!(
                        "program {prog} round {round} budget {budget}: {d}: {}",
                        words.join(" ")
                    ));
                }
                break;
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Programs of the system instructions that the loop runs without leaving it (MRS, MSR to DAIFSet), among
/// those that leave it (MSR of registers, DAIFClr, SPSel, barriers), at EL0 and EL1, leave what `execute`
/// leaves, one instruction at a time (the ones the loop runs raise the same exceptions at the same pc).
#[test]
fn the_loop_agrees_with_execute_on_system_sequences() {
    use crate::sysreg as sr;
    const N: usize = 16;
    const KEYS: [u32; 22] = [
        sr::MIDR_EL1,
        sr::SCTLR_EL1,
        sr::TTBR0_EL1,
        sr::TCR_EL1,
        sr::ELR_EL1,
        sr::SP_EL0,
        sr::SPSEL,
        sr::CURRENT_EL,
        sr::NZCV,
        sr::DAIF,
        sr::FPCR,
        sr::FPSR,
        sr::ESR_EL1,
        sr::VBAR_EL1,
        sr::TPIDR_EL1,
        sr::TPIDR_EL0,
        sr::TPIDRRO_EL0,
        sr::CNTVCT_EL0,
        sr::CNTFRQ_EL0,
        sr::CNTV_CTL_EL0,
        sr::CNTV_TVAL_EL0,
        sr::ID_AA64MMFR0_EL1,
    ];
    let mut rng = Rng(0x1357_9bdf_2468_ace0);
    let mut bad = Vec::new();
    for prog in 0..300 {
        let code: Vec<u32> = (0..N)
            .map(|_| {
                let rt = (rng.next() % 32) as u32;
                let key = KEYS[(rng.next() % KEYS.len() as u64) as usize];
                let crm = ((rng.next() % 16) as u32) << 8;
                match rng.next() % 9 {
                    0..=3 => 0xd520_0000 | key << 5 | rt,
                    // a write of the registers that move the code (MMU, vector base) would end the program
                    4 => {
                        let key = match key {
                            sr::SCTLR_EL1 | sr::TTBR0_EL1 | sr::TCR_EL1 | sr::VBAR_EL1 => sr::TPIDR_EL1,
                            k => k,
                        };
                        0xd500_0000 | key << 5 | rt
                    },
                    5 => 0xd503_40df | crm,
                    6 => [0xd503_40ff, 0xd500_40bf][(rng.next() % 2) as usize] | crm,
                    7 => 0xd503_3fdf,
                    _ => 0xd503_201f,
                }
            })
            .collect();
        let mut a = Cpu::new(4 << 20, NoDevices);
        let mut b = Cpu::new(4 << 20, NoDevices);
        for (j, w) in code.iter().enumerate() {
            for c in [&mut a, &mut b] {
                c.mem.write(0x1000 + 4 * j, &w.to_le_bytes());
            }
        }
        for round in 0..4 {
            let budget = if round == 0 { N as u64 } else { 1 + rng.next() % N as u64 };
            for r in 0..31 {
                let v = rng.reg();
                a.x[r] = v;
                b.x[r] = v;
            }
            let (el, spsel, daif, uma, f) = (
                (rng.next() & 1) as u8,
                rng.next() & 1 != 0,
                (rng.next() as u32 & 15) << 6,
                rng.next() & 1,
                rng.next(),
            );
            for c in [&mut a, &mut b] {
                c.el = el;
                c.spsel = spsel;
                c.daif = daif;
                c.sys.sctlr = uma << 9;
                c.n = f & 1 != 0;
                c.z = f & 2 != 0;
                c.c = f & 4 != 0;
                c.v = f & 8 != 0;
                c.pc = 0x1000;
            }
            a.run(budget);
            for _ in 0..budget {
                let pc = b.pc;
                let insn =
                    b.mem.load(pc as usize, 4) as u32;
                b.next_pc = pc + 4;
                match b.execute(insn).map(|()| b.next_pc) {
                    Ok(next) => b.pc = next,
                    Err(ev) => {
                        let _ = b.step_event(pc, ev);
                    },
                }
            }
            let diff = if a.x[..32] != b.x[..32] {
                let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
                Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
            } else if (a.pc, a.el, a.daif, a.spsel) != (b.pc, b.el, b.daif, b.spsel) {
                Some(format!(
                    "pc/el/daif/spsel {:#x} {} {:#x} {} against {:#x} {} {:#x} {}",
                    a.pc, a.el, a.daif, a.spsel, b.pc, b.el, b.daif, b.spsel
                ))
            } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
                Some("flags".to_string())
            } else if (
                a.sp_el0,
                a.sp_el1,
                a.sys.tpidr_el0,
                a.sys.tpidr_el1,
                a.sys.fpcr,
                a.sys.fpsr,
            ) != (
                b.sp_el0,
                b.sp_el1,
                b.sys.tpidr_el0,
                b.sys.tpidr_el1,
                b.sys.fpcr,
                b.sys.fpsr,
            ) {
                Some("registers".to_string())
            } else if (a.sys.esr, a.sys.elr, a.sys.far) != (b.sys.esr, b.sys.elr, b.sys.far) {
                Some("esr/elr/far".to_string())
            } else {
                None
            };
            if let Some(d) = diff {
                if bad.len() < 20 {
                    let words: Vec<String> = code.iter().map(|w| format!("{w:08x}")).collect();
                    bad.push(format!(
                        "program {prog} round {round} budget {budget} el {el}: {d}: {}",
                        words.join(" ")
                    ));
                }
                break;
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ------------------------------------------------------------ programs of several pages

/// A host with a JIT that takes the compile requests and never reports one ready, as a compile that is
/// slow does: the entries it was asked for stay in flight, the interpreter goes on running the code, and
/// `jit_step` is refused the next compile of them.
struct Takes;
impl Host for Takes {
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
    fn jit_compile(&mut self, _: u32, _: &[u8]) -> bool {
        true
    }
}

/// What the machine under test has for a JIT.
#[derive(Clone, Copy, PartialEq)]
enum Tier {
    /// None: the loop only.
    Off,
    /// The one of `Takes`: it counts the entries and has compiles asked for, which never get ready.
    Stub,
    /// The native one, which compiles the hot entries, so that the loop finds modules to run.
    Native,
}

/// The code of the programs: `PAGES` pages from `CODE`, the machine's memory being the address space.
const CODE: u64 = 0x10000;
const PAGES: usize = 4;

fn put<H: Host>(c: &mut Cpu<H>, at: u64, word: u32) {
    c.mem.write(at as usize, &word.to_le_bytes());
}

/// `B to` at address `from`.
fn branch(from: u64, to: u64) -> u32 {
    0x1400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x3ff_ffff)
}

/// The instruction at the pc of `b`, run by `execute`, which is how the loop is held to what it does: the
/// MMU is off in the machine, so the pc is the address in memory.
fn step_ref<H: Host>(b: &mut Cpu<H>) {
    let pc = b.pc;
    let insn = b.mem.load(pc as usize, 4) as u32;
    b.next_pc = pc + 4;
    match b.execute(insn).map(|()| b.next_pc) {
        Ok(next) => b.pc = next,
        Err(ev) => {
            let _ = b.step_event(pc, ev);
        },
    }
}

/// What differs between two machines that ran the same instructions, if anything.
fn differ<H: Host, G: Host>(a: &Cpu<H>, b: &Cpu<G>) -> Option<String> {
    if a.x[..32] != b.x[..32] {
        let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
        Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
    } else if (a.sp_el0, a.sp_el1) != (b.sp_el0, b.sp_el1) {
        Some("sp".to_string())
    } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
        Some("flags".to_string())
    } else if a.pc != b.pc || a.el != b.el {
        Some(format!("pc {:#x} against {:#x}", a.pc, b.pc))
    } else if (a.sys.esr, a.sys.elr, a.sys.far) != (b.sys.esr, b.sys.elr, b.sys.far) {
        Some(format!(
            "esr/elr/far {:#x} {:#x} {:#x} against {:#x} {:#x} {:#x}",
            a.sys.esr, a.sys.elr, a.sys.far, b.sys.esr, b.sys.elr, b.sys.far
        ))
    } else if a.vreg != b.vreg {
        Some("vreg".to_string())
    } else {
        None
    }
}

impl Rng {
    /// A word of a program of `n` words for the branch at word `j` to go to: near it half of the time,
    /// else anywhere in the program (so, mostly, in another page).
    fn target(&mut self, j: usize, n: usize) -> usize {
        if self.next() & 1 != 0 {
            let t = j as i64 + (self.next() % 129) as i64 - 64;
            t.clamp(0, n as i64 - 1) as usize
        } else {
            (self.next() % n as u64) as usize
        }
    }

    /// A program of `PAGES` pages: what `flag_program` writes, with branches of every kind in the place of
    /// some of its instructions, to words anywhere in the program, backwards too: B, BL, B.cond, CBZ and
    /// CBNZ, TBZ and TBNZ, RET (x30), and BR, BLR and RET (x17) behind the MOVZ and MOVK that give x17 the
    /// address of a word. The last word goes back to the first.
    fn paged_program(&mut self) -> Vec<u32> {
        let n = PAGES * 1024;
        let mut code = self.flag_program(n);
        let mut j = 0usize;
        loop {
            j += 1 + (self.next() % 10) as usize;
            if j + 3 >= n - 1 {
                break;
            }
            let t = self.target(j, n);
            let off = t as i64 - j as i64;
            let imm26 = (off as u32) & 0x3ff_ffff;
            let imm19 = ((off as u32) & 0x7_ffff) << 5;
            let imm14 = ((off as u32) & 0x3fff) << 5;
            let rt = self.small_reg();
            match self.next() % 14 {
                0..=2 => code[j] = 0x1400_0000 | imm26,
                3 => code[j] = 0x9400_0000 | imm26,
                4 => code[j] = 0x5400_0000 | imm19 | (self.next() % 16) as u32,
                5..=6 => {
                    let op = [0xb400_0000u32, 0xb500_0000, 0x3400_0000, 0x3500_0000];
                    code[j] = op[(self.next() % 4) as usize] | imm19 | rt;
                },
                7 => {
                    let bit = (self.next() % 64) as u32;
                    let op = [0x3600_0000u32, 0x3700_0000][(self.next() % 2) as usize];
                    code[j] = op | ((bit >> 5) << 31) | ((bit & 31) << 19) | imm14 | rt;
                },
                8 => code[j] = 0xd65f_03c0,
                9..=11 => {
                    let va = CODE + 4 * t as u64;
                    code[j] = 0xd280_0000 | (((va & 0xffff) as u32) << 5) | 17;
                    code[j + 1] = 0xf2a0_0000 | ((((va >> 16) & 0xffff) as u32) << 5) | 17;
                    let op = [0xd61f_0220u32, 0xd63f_0220, 0xd65f_0220];
                    code[j + 2] = op[(self.next() % 3) as usize];
                    j += 2;
                },
                _ => {},
            }
        }
        code[n - 1] = 0x1400_0000 | ((1 - n as i64) as u32 & 0x3ff_ffff);
        code
    }
}

/// Programs of several pages, run by the loop in rounds from random words, with budgets around the length
/// of a run (`PAGE_RUN`) and short ones, leave what `execute` leaves, one instruction at a time: registers,
/// flags and pc, wherever the run ends (in the middle of a page, between a compare and its branch, in the
/// first instruction of a page that a branch led to), and the budget is the exact one.
fn paged_vs_execute(tier: Tier, seed: u64, programs: usize) {
    const EDGES: [u64; 8] = [
        0,
        1,
        2,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x7fff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
    ];
    let n = (PAGES * 1024) as u64;
    let mut rng = Rng(seed);
    let (mut bad, mut changes) = (Vec::new(), 0u64);
    'programs: for prog in 0..programs {
        let code = rng.paged_program();
        let mut a = Cpu::new(4 << 20, Takes);
        let mut b = Cpu::new(4 << 20, Takes);
        a.sys.cpacr = 3 << 20;
        b.sys.cpacr = 3 << 20;
        match tier {
            Tier::Off => {},
            Tier::Stub => {
                a.jit.enabled = true;
                a.jit.threshold = 16;
            },
            Tier::Native => {
                #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
                {
                    if !a.enable_native_jit() {
                        return; // no Cranelift backend for this host
                    }
                    a.jit.threshold = 4;
                }
            },
        }
        for (j, w) in code.iter().enumerate() {
            put(&mut a, CODE + 4 * j as u64, *w);
            put(&mut b, CODE + 4 * j as u64, *w);
        }
        for round in 0..5 {
            let budget = match round {
                0 => 255,
                1 => 256,
                2 => 257,
                3 => 1 + rng.next() % 40,
                _ => 600 + rng.next() % 900,
            };
            let start = CODE + 4 * (rng.next() % n);
            let (r17, r30) = (CODE + 4 * (rng.next() % n), CODE + 4 * (rng.next() % n));
            let (s0, s1, f) = (rng.reg(), rng.reg(), rng.next());
            for r in 0..8 {
                let v = match rng.next() & 1 {
                    0 => EDGES[(rng.next() % 8) as usize],
                    _ => rng.reg(),
                };
                a.x[r] = v;
                b.x[r] = v;
            }
            for c in [&mut a, &mut b] {
                c.x[17] = r17;
                c.x[30] = r30;
                c.sp_el0 = s0;
                c.sp_el1 = s1;
                c.n = f & 1 != 0;
                c.z = f & 2 != 0;
                c.c = f & 4 != 0;
                c.v = f & 8 != 0;
                c.pc = start;
            }
            let before = a.insn_count;
            let exit = a.run(budget);
            // The modules of the native JIT that the run asked for are there for the next one.
            #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
            a.wait_native_jit();
            let ran = a.insn_count - before;
            for _ in 0..ran {
                let was = b.pc;
                step_ref(&mut b);
                changes += ((was ^ b.pc) >> 12 != 0) as u64;
            }
            // With no vectors installed an exception stops the machine, which leaves the pc where the
            // run began (nothing goes on from there): what it raised is what is compared.
            let stopped = matches!(exit, Exit::Stop(_));
            if stopped {
                a.pc = b.pc;
            }
            // A module may end a run past the budget, by what is left of its block.
            let diff = if tier != Tier::Native && ran != budget && !stopped {
                Some(format!("ran {ran} of the budget: {exit:?}, esr {:#x}", a.sys.esr))
            } else {
                differ(&a, &b)
            };
            if let Some(d) = diff {
                if bad.len() < 20 {
                    bad.push(format!(
                        "tier {} program {prog} round {round} budget {budget} at {start:#x}: {d}",
                        tier as u8
                    ));
                }
                continue 'programs;
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert!(changes > programs as u64 * 10, "only {changes} branches to another page");
}

#[test]
fn the_loop_agrees_with_execute_on_programs_of_several_pages() {
    paged_vs_execute(Tier::Off, 0x2f6e_2b1c_90a3_5d47, 60);
}

#[test]
fn the_loop_agrees_with_execute_on_programs_of_several_pages_with_compiles_in_flight() {
    paged_vs_execute(Tier::Stub, 0x51ed_270a_c4b3_91f5, 40);
}

#[test]
#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
fn the_loop_agrees_with_execute_on_programs_of_several_pages_with_the_native_jit() {
    paged_vs_execute(Tier::Native, 0x7c3a_9e15_b2d8_4f61, 25);
}

/// Two pages that branch to each other, run with every budget up to a few laps: the budget is exact, and
/// leaves what `execute` leaves, wherever it ends in a page. With both pages held and decoded, one call of
/// `step_page` goes through all of them (and not out of the page that holds its first instruction).
#[test]
fn a_run_goes_on_in_the_page_a_taken_branch_leads_to() {
    for tier in [Tier::Off, Tier::Stub] {
        let mut a = Cpu::new(4 << 20, Takes);
        let mut b = Cpu::new(4 << 20, Takes);
        if tier == Tier::Stub {
            a.jit.enabled = true;
            a.jit.threshold = 16;
        }
        let (p0, p1) = (CODE, CODE + 0x1000);
        for c in [&mut a, &mut b] {
            for k in 0..6u64 {
                put(c, p0 + 4 * k, 0x9100_0400);
            }
            put(c, p0 + 24, branch(p0 + 24, p1));
            for k in 0..5u64 {
                put(c, p1 + 4 * k, 0x9100_0c21);
            }
            put(c, p1 + 20, branch(p1 + 20, p0));
            c.pc = p0;
        }
        for budget in 1..=40u64 {
            let before = a.insn_count;
            a.run(budget);
            assert_eq!(a.insn_count - before, budget);
            for _ in 0..budget {
                step_ref(&mut b);
            }
            assert_eq!(differ(&a, &b), None, "budget {budget}");
        }
        if tier == Tier::Off {
            for c in [&mut a, &mut b] {
                c.pc = p0;
            }
            let (r, n) = a.step_page(p0, 100);
            assert_eq!(n, 100);
            assert!(matches!(r, Ok(pc) if pc == a.pc));
            for _ in 0..100 {
                step_ref(&mut b);
            }
            assert_eq!(differ(&a, &b), None);
        }
    }
}

/// A store into the code of the page the run goes on in, which the run has gone through before, changes
/// what it runs there: the store drops the decoded page, and a run that went on with the ops it had would
/// run the instruction that is not there any more.
#[test]
fn a_store_into_the_next_page_changes_what_the_run_does_there() {
    for tier in [Tier::Off, Tier::Stub] {
        let mut a = Cpu::new(4 << 20, Takes);
        let mut b = Cpu::new(4 << 20, Takes);
        if tier == Tier::Stub {
            a.jit.enabled = true;
            a.jit.threshold = 16;
        }
        let (p0, p1) = (CODE, CODE + 0x1000);
        for c in [&mut a, &mut b] {
            // str w1, [x2]; add w1, w1, w4; add x5, x5, #1; b p1
            put(c, p0, 0xb900_0041);
            put(c, p0 + 4, 0x0b04_0021);
            put(c, p0 + 8, 0x9100_04a5);
            put(c, p0 + 12, branch(p0 + 12, p1));
            // add x0, x0, #1 nine times, the fourth one the instruction that is stored over; b p0
            for k in 0..9u64 {
                put(c, p1 + 4 * k, 0x9100_0400);
            }
            put(c, p1 + 36, branch(p1 + 36, p0));
            // The stored word is add x0, x0, #imm with imm one more each time.
            c.x[1] = 0x9100_0800;
            c.x[2] = p1 + 12;
            c.x[4] = 0x400;
            c.pc = p0;
        }
        for round in 0..4 {
            a.run(700);
            for _ in 0..700 {
                step_ref(&mut b);
            }
            assert_eq!(differ(&a, &b), None, "round {round}");
            let same = a.mem.to_vec()[0x10000..0x12000] == b.mem.to_vec()[0x10000..0x12000];
            assert!(same, "round {round}: memory");
            assert!(a.x[5] >= 40 * (round + 1), "round {round}: only {} laps", a.x[5]);
        }
    }
}

/// Compiled loads and stores that cross from a page into the next, where the next virtual page is mapped
/// elsewhere (0x11 to 0x15): the fast table holds the first page, and an access whose last byte is in the
/// next one must take the slow path, which translates each page; served from the first page's host memory
/// it would read and write the physical page after it (0x11) instead.
#[test]
#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
fn compiled_accesses_that_cross_into_a_page_mapped_elsewhere_reach_it() {
    let mut a = mapped();
    let mut b = mapped();
    if !a.enable_native_jit() {
        return; // no Cranelift backend for this host
    }
    a.jit.threshold = 2;
    let code = 0x5000u64;
    for c in [&mut a, &mut b] {
        map(c, 0x11, (0x15 << 12) | 0x403);
        for (base, fill) in [(0x10000usize, 0x10u8), (0x11000, 0x77), (0x15000, 0x55)] {
            let bytes: Vec<u8> = (0..0x1000).map(|i| fill ^ (i as u8)).collect();
            c.mem.write(base, &bytes);
        }
        // ldr x5, [x1]; ldrh w7, [x3]; str x6, [x2]; strh w6, [x4]; add x6, x6, #1; b code
        for (k, w) in [0xf940_0025u32, 0x7940_0067, 0xf900_0046, 0x7900_0086, 0x9100_04c6].iter().enumerate() {
            put(c, code + 4 * k as u64, *w);
        }
        put(c, code + 20, branch(code + 20, code));
        c.pc = code;
        c.x[6] = 0x0123_4567_89ab_cdef;
    }
    // Inside the page first: compiled, and the page in the fast table; then across into the next
    for (round, back) in [0x20u64, 0x18, 1, 2, 3, 4, 5, 6, 7].into_iter().enumerate() {
        for c in [&mut a, &mut b] {
            c.x[1] = 0x11000 - back;
            c.x[2] = 0x11000 - back.min(7);
            c.x[3] = 0x11000 - back.min(1);
            c.x[4] = 0x11000 - 1;
        }
        let before = a.insn_count;
        a.run(120);
        a.wait_native_jit();
        for _ in 0..a.insn_count - before {
            step_ref(&mut b);
        }
        assert_eq!(differ(&a, &b), None, "round {round}");
        for page in [0x10000usize, 0x11000, 0x15000] {
            let same = a.mem.to_vec()[page..page + 0x1000] == b.mem.to_vec()[page..page + 0x1000];
            assert!(same, "round {round}: memory at {page:#x}");
        }
    }
    assert!(a.jit.block_runs > 0, "nothing ran compiled");
}

/// A machine with the MMU on: a space of 39 bits, 4 KiB pages, one table for each of the three levels (at
/// 0x80000, 0x81000, 0x82000), the first 0x20 pages mapped to themselves, EL1, the vectors in page 0 where
/// `b .` stands, so that a fault leaves the machine spinning.
fn mapped() -> Cpu<Takes> {
    let mut c = Cpu::new(4 << 20, Takes);
    c.sys.cpacr = 3 << 20;
    c.sys.sctlr |= 1;
    c.sys.tcr = 25;
    c.sys.ttbr0 = 0x80000;
    c.mem.write(0x80000, &(0x81000u64 | 3).to_le_bytes());
    c.mem.write(0x81000, &(0x82000u64 | 3).to_le_bytes());
    for page in 0..0x20u64 {
        map(&mut c, page, (page << 12) | 0x403);
    }
    put(&mut c, 0x0, 0x1400_0000);
    put(&mut c, 0x200, 0x1400_0000);
    c
}

/// The descriptor of the level 3 table for virtual page `page`.
fn map(c: &mut Cpu<Takes>, page: u64, desc: u64) {
    let at = 0x82000 + 8 * page as usize;
    c.mem.write(at, &desc.to_le_bytes());
}

/// `a` runs `n` instructions in one call, `b` one at a time (so that no run of it goes from a page to
/// another): they leave the same state, and the same count.
fn lockstep(a: &mut Cpu<Takes>, b: &mut Cpu<Takes>, n: u64, why: &str) {
    a.run(n);
    for _ in 0..n {
        let _ = b.run(1);
    }
    assert_eq!(a.insn_count, b.insn_count, "{why}");
    assert_eq!(differ(a, b), None, "{why}");
}

/// A taken branch to a page whose fetch faults raises what that fetch raises, at the target: the page was
/// unmapped or not executable, or the translation of it changed, and the TLB was maintained for the page,
/// or all of it (when it was not, the stale translation is what the machine still uses, as it is one
/// instruction at a time). A target that is not aligned raises the alignment fault of the fetch.
#[test]
fn a_branch_to_a_page_that_the_fetch_faults_on_raises_that_fault() {
    const T: u64 = 0x11000;
    // The branch to T that follows `add x0, x0, #1` at the start of page 0x10: x1 is 7, x17 and x30 are T.
    let kinds: [u32; 7] = [
        0x1400_0000 | 0x3ff,                    // b
        0x5400_0001 | (0x3ff << 5),             // b.ne
        0xb500_0001 | (0x3ff << 5),             // cbnz x1
        0x3700_0001 | (1 << 19) | (0x3ff << 5), // tbnz x1, #1
        0xd61f_0220,                            // br x17
        0xd63f_0220,                            // blr x17
        0xd65f_03c0,                            // ret
    ];
    for (ki, &kind) in kinds.iter().enumerate() {
        for mode in 0..3 {
            for maint in 0..3 {
                let why = format!("branch {ki} mode {mode} maintenance {maint}");
                let mut a = mapped();
                let mut b = mapped();
                for c in [&mut a, &mut b] {
                    put(c, 0x10000, 0x9100_0400);
                    put(c, 0x10004, kind);
                    put(c, 0x10008, branch(0x10008, 0x10000));
                    // The page the branches lead to, and the one it is mapped to after.
                    put(c, 0x11000, 0x9100_0442); // add x2, x2, #1
                    put(c, 0x11004, branch(0x11004, 0x10000));
                    put(c, 0x13000, 0x9100_0463); // add x3, x3, #1
                    // (the page runs at 0x11000, mapped to this: its branch is the one of address 0x11004)
                    put(c, 0x13004, branch(0x11004, 0x10000));
                    c.x[1] = 7;
                    c.x[17] = T;
                    c.x[30] = T;
                    c.pc = 0x10000;
                }
                lockstep(&mut a, &mut b, 60, &why);
                let laps = a.x[2];
                assert!(laps > 5, "{why}: the page ran {laps} times");
                for c in [&mut a, &mut b] {
                    match mode {
                        0 => map(c, 0x11, 0),
                        1 => map(c, 0x11, 0x11000 | 0x403 | (1 << 53)),
                        _ => map(c, 0x11, 0x13000 | 0x403),
                    }
                    match maint {
                        0 => c.tlb_flush_page(0x11),
                        1 => c.tlb_flush(),
                        _ => {},
                    }
                    c.x[17] = T;
                    c.x[30] = T;
                    c.pc = 0x10000;
                }
                lockstep(&mut a, &mut b, 60, &why);
                if maint == 2 {
                    continue;
                }
                match mode {
                    0 | 1 => {
                        assert_eq!(a.sys.esr >> 26, 0x21, "{why}");
                        assert_eq!((a.sys.far, a.sys.elr), (T, T), "{why}");
                        assert_eq!(a.x[2], laps, "{why}: ran the page that faults");
                    },
                    _ => {
                        assert!(a.x[3] > 5, "{why}: the new page did not run");
                        assert_eq!(a.x[2], laps, "{why}: ran the old page");
                    },
                }
            }
        }
    }
    for (ki, &kind) in kinds[4..].iter().enumerate() {
        let why = format!("misaligned branch {ki}");
        let mut a = mapped();
        let mut b = mapped();
        for c in [&mut a, &mut b] {
            put(c, 0x10000, 0x9100_0400);
            put(c, 0x10004, kind);
            put(c, 0x11000, 0x9100_0442);
            c.x[17] = T + 2;
            c.x[30] = T + 2;
            c.pc = 0x10000;
        }
        lockstep(&mut a, &mut b, 20, &why);
        assert_eq!(a.sys.esr >> 26, 0x22, "{why}");
        assert_eq!((a.sys.far, a.sys.elr), (T + 2, T + 2), "{why}");
        assert_eq!(a.x[2], 0, "{why}: ran the page");
    }
}

/// The entry of a loop that the JIT refused (a compile that has it is in flight) is not sent back to
/// `jit_step` by each execution of it: its count is left below the threshold, from where it gets there
/// again after some executions. The loop is run as `execute` runs it.
#[test]
fn an_entry_that_the_jit_refused_is_counted_again_from_below_the_threshold() {
    let mut a = Cpu::new(4 << 20, Takes);
    let mut b = Cpu::new(4 << 20, Takes);
    a.jit.enabled = true;
    a.jit.threshold = 64;
    // add x1, x1, #1; subs x0, x0, #1; b.ne the add; b .
    let code = [0x9100_0421u32, 0xf100_0400, 0x54ff_ffc1, 0x1400_0000];
    for c in [&mut a, &mut b] {
        for (k, w) in code.iter().enumerate() {
            put(c, CODE + 4 * k as u64, *w);
        }
        c.x[0] = 3000;
        c.pc = CODE;
    }
    let steps = 3 * 3000 + 1;
    a.run(steps);
    for _ in 0..steps {
        step_ref(&mut b);
    }
    assert_eq!(a.insn_count, steps);
    assert_eq!(differ(&a, &b), None);
    assert_eq!(a.x[1], 3000);
    assert!(a.jit.compiled >= 1, "the loop was never compiled");
    let ops = a.jit.pre.enter(CODE as usize);
    assert!(!ops.is_null());
    // SAFETY: `ops` is the ops of the page at CODE, a live slot of 1024 ops or more.
    let hottest = (0..1024).map(|i| unsafe { (*ops.add(i)).cnt }).max().unwrap();
    assert!(hottest < 64, "the count of the entry is {hottest}: each execution exits");
}
