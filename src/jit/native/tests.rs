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

const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
const EXIT: u64 = 0x5000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Non-branch encoding groups, so that every block ends in its own final branch.
const BODY: &[(u32, u32)] = &[
    (0x1c00_0000, 0x1000_0000), // data processing (immediate)
    (0x0e00_0000, 0x0a00_0000), // data processing (register)
    (0x0a00_0000, 0x0800_0000), // loads and stores
    (0x3b00_0000, 0x3900_0000), // LDR/STR unsigned offset
    (0x3b20_0000, 0x3800_0000), // LDUR/STUR, pre/post, LDTR
    (0x3a00_0000, 0x2800_0000), // LDP/STP
    (0x0e00_0000, 0x0e00_0000), // SIMD/FP
    (0x3f00_0000, 0x3d00_0000), // LDR/STR (SIMD&FP) unsigned offset
    (0x3f20_0000, 0x3c00_0000), // LDUR/STUR (SIMD&FP), pre/post
    (0x3f20_0c00, 0x3c20_0800), // LDR/STR (SIMD&FP) register offset
    (0x3e00_0000, 0x2c00_0000), // LDP/STP (SIMD&FP)
    (0x9f20_0c00, 0x0e20_0000), // three different: SMULL, SMLAL, ... (and what stays interpreted)
    (0x9f3f_0c00, 0x0e20_0800), // two-register misc: SADDLP, SADALP, ...
    (0x9f20_0400, 0x0e20_0400), // three same: ADDP, ...
    (0xbf20_8c00, 0x0e00_0800), // UZP, TRN, ZIP
    (0xffa0_0c00, 0x1e20_0800), // FP data-processing (2 source), single and double
    (0xff80_0000, 0x1f00_0000), // FMADD, FMSUB, FNMADD, FNMSUB
    (0xff20_7c00, 0x1e20_4000), // FP data-processing (1 source)
    (0xff20_fc07, 0x1e20_2000), // FCMP, FCMPE
    (0xff20_0c00, 0x1e20_0c00), // FCSEL
    (0xff20_1fe0, 0x1e20_1000), // FMOV (immediate)
    (0x7f20_fc00, 0x1e20_0000), // conversions between FP and integer
    (0xdfff_fc00, 0x5e21_d800), // SCVTF / UCVTF (scalar, 32-bit)
    (0xdfff_fc00, 0x5ea1_b800), // FCVTZS / FCVTZU (scalar, 32-bit)
    (0xffe0_fc00, 0x5e00_0400), // DUP (element, scalar)
];

/// Narrow groups `BODY` rarely draws (each is a sliver of its class): what a refactor of the
/// translator is likeliest to break without the wide fuzzing noticing.
const NARROW: &[(u32, u32)] = &[
    (0x7fff_e000, 0x5ac0_0000), // RBIT, REV16, REV32, REV, CLZ, CLS (and unallocated)
    (0xff60_fc00, 0x9b40_7c00), // SMULH, UMULH
    (0x7fe0_fc00, 0x1ac0_2c00), // RORV
    (0x7fa0_0000, 0x1380_0000), // EXTR (ROR immediate)
    (0x7fe0_f000, 0x1ac0_2000), // LSLV, LSRV, ASRV, RORV
    (0xbfe0_fc00, 0x0e00_0c00), // DUP (general)
    (0xbf3f_fc00, 0x0e21_2800), // XTN, XTN2
    (0x3f00_0000, 0x0800_0000), // load/store exclusive, acquire/release, CAS
    (0x3f20_0c00, 0x3820_0000), // LSE atomics: LDADD, LDCLR, LDEOR, LDSET, SWP, ...
    (0xbf20_8400, 0x2e00_0000), // EXT
    // (U free: CMGE and CMLE are the U forms)
    (0x9f3f_fc00, 0x0e20_8800), // CMGT, CMGE (zero)
    (0x9f3f_fc00, 0x0e20_9800), // CMEQ, CMLE (zero)
    (0x9f3f_fc00, 0x0e20_a800), // CMLT (zero)
    (0x9f20_c400, 0x0e20_c400), // three same, floating point: FMLA, FADD, FMUL, FDIV, ...
];

/// A small number with few significant bits: sums, products and quotients of these are
/// often exact.
fn fp_small(rng: &mut Rng, double: bool) -> u64 {
    let v = (rng.below(65) as f64 - 32.0) / [1.0, 2.0, 4.0, 8.0][rng.below(4) as usize];
    if double {
        v.to_bits()
    } else {
        (v as f32).to_bits() as u64
    }
}

/// A value worth computing with in the low lanes: mostly ordinary numbers whose results are
/// often exact, sometimes the edges (zeros, infinities, NaNs, subnormals, extremes).
fn fp_value(rng: &mut Rng, double: bool) -> u64 {
    let f = |v: f64| if double { v.to_bits() } else { (v as f32).to_bits() as u64 };
    match rng.below(10) {
        0 => f(rng.below(33) as f64 - 16.0),
        1 => f((rng.below(257) as f64 - 128.0) / 8.0),
        2 => f((rng.below(1 << 20) as f64 - (1 << 19) as f64) * 1e-3),
        3 => {
            let edges: [u64; 10] = if double {
                [
                    0,
                    1 << 63,
                    0x7ff0_0000_0000_0000,
                    0xfff0_0000_0000_0000,
                    0x7ff8_0000_0000_0000,
                    0x7ff0_0000_0000_0001,
                    1,
                    0x0010_0000_0000_0000,
                    0x7fef_ffff_ffff_ffff,
                    0x000f_ffff_ffff_ffff,
                ]
            } else {
                [
                    0,
                    0x8000_0000,
                    0x7f80_0000,
                    0xff80_0000,
                    0x7fc0_0000,
                    0x7f80_0001,
                    1,
                    0x0080_0000,
                    0x7f7f_ffff,
                    0x007f_ffff,
                ]
            };
            edges[rng.below(10) as usize]
        },
        4 => {
            // near the smallest normal and the largest finite
            let e = if rng.below(2) == 0 {
                1 + rng.below(3)
            } else {
                (if double { 0x7fd } else { 0xfc }) + rng.below(3)
            };
            let (shift, mant) = if double {
                (52, rng.next() & ((1 << 52) - 1))
            } else {
                (23, rng.next() & 0x7f_ffff)
            };
            (rng.below(2) << if double { 63 } else { 31 }) | (e << shift) | mant
        },
        _ => {
            // any normal with a moderate exponent
            let e = rng.below(60) as i64 - 30;
            let (bias, shift, mant) = if double {
                (1023, 52, rng.next() & ((1 << 52) - 1))
            } else {
                (127, 23, rng.next() & 0x7f_ffff)
            };
            (rng.below(2) << if double { 63 } else { 31 })
                | (((bias + e) as u64) << shift)
                | mant
        },
    }
}

/// Everything a block can change: registers, flags, exception state and RAM.
fn state(c: &Cpu<NoDevices>) -> impl PartialEq + std::fmt::Debug {
    (
        c.x[..31].to_vec(),
        c.pc,
        (c.sp_el0, c.sp_el1, c.el, c.spsel, c.daif),
        (c.n, c.z, c.c, c.v),
        (c.sys.esr, c.sys.far, c.sys.elr, c.sys.spsr),
        c.vreg,
        (c.sys.fpcr, c.sys.fpsr),
    )
}

/// Random blocks of random instructions from random states, run by the interpreter and as
/// native code: registers, flags, exception state and RAM must match.
/// Three-same floating point on .2D only, where FADD, FSUB, FMUL, FDIV, FMLA and FMLS run
/// inline once FPSR.IXC is set (`Gen::fp_2d`): results and flags must still match.
#[test]
fn double_vector_fp_does_what_the_interpreter_does() {
    fuzz(0x9e37_79b9_7f4a_7c15, 6000, &[(0xdf60_c400, 0x4e60_c400)], true);
}

/// Scalar double FMUL, FNMUL, FDIV (with the rest of that encoding group) and FMADD,
/// FMSUB, FNMADD, FNMSUB, inline once FPSR.IXC is set (`Gen::fp_double_sticky`).
#[test]
fn double_scalar_fp_does_what_the_interpreter_does() {
    let body = [(0xffe0_0c00, 0x1e60_0800), (0xffc0_0000, 0x1f40_0000)];
    fuzz(0x6a09_e667_f3bc_c908, 6000, &body, true);
}
#[test]
fn compiled_blocks_do_what_the_interpreter_does() {
    fuzz(0x2545_f491_4f6c_dd1d, 6000, BODY, false);
}

/// Indirect branches (BLR, RET) within a page continue in compiled code; across pages, to an
/// address no block starts at, or misaligned (a PC alignment fault), they must still do
/// exactly what the interpreter does.
#[test]
fn indirect_branches_do_what_the_interpreter_does() {
    // CODE:     blr x1        ; to x1
    // CODE+4:   b EXIT
    // CODE+8:   add x0, x0, #1
    // CODE+12:  ret           ; to CODE+4
    // VECTORS + 0x200 (synchronous, from EL1 with SP_EL1): b .
    const VECTORS: u64 = 0x3000;
    let words = [
        (CODE, 0xd63f_0020u32),
        (CODE + 4, 0x1400_0000 | ((EXIT - CODE - 4) / 4) as u32),
        (CODE + 8, 0x9100_0400),
        (CODE + 12, 0xd65f_03c0),
        (EXIT, 0x1400_0000),
        (VECTORS + 0x200, 0x1400_0000),
    ];
    let target_page = 0x4000;
    let cases = [
        ("same page", CODE + 8),
        ("misaligned, same page", CODE + 10),
        ("another page", target_page),
        ("misaligned, another page", target_page + 2),
    ];
    for (what, target) in cases {
        let setup = |c: &mut Cpu<NoDevices>| {
            for &(a, w) in &words {
                c.mem.write(a as usize, &w.to_le_bytes());
            }
            // At the other page: add x0, x0, #1; ret
            c.mem
                .write(target_page as usize, &0x9100_0400u32.to_le_bytes());
            c.mem
                .write(target_page as usize + 4, &0xd65f_03c0u32.to_le_bytes());
            (c.el, c.spsel, c.daif) = (1, true, 0x3c0);
            c.sys.vbar = VECTORS;
        };
        let mut interp = Cpu::new(RAM, NoDevices);
        setup(&mut interp);
        let Some(mut jit) = at_block(1) else { return };
        setup(&mut jit);
        // The page compiled with every entry at once, so that they chain to one another
        let mut page = [0u8; 0x1000];
        jit.mem.read(CODE as usize & !0xfff, &mut page);
        let key = ((CODE & !0xfff) << 2) | 0b11; // EL1, SP_EL1
        let entries = vec![0, 4, 8, 12];
        let hash = crate::jit::profile::hash(&page);
        let prewarm = [crate::jit::profile::Page {
            key,
            hash,
            runs: 1,
            entries,
        }];
        assert_eq!(jit.jit_prewarm(&prewarm), 1, "{what}");
        jit.wait_native_jit();
        // First to CODE+8, so that it is an entry by the time `target` (perhaps next to it)
        // is taken
        for round in 0..6 {
            for c in [&mut interp, &mut jit] {
                c.x[1] = if round < 3 { CODE + 8 } else { target };
                c.x[0] = round;
                c.pc = CODE;
                c.jit_entry = true;
                c.run(40);
            }
            jit.wait_native_jit();
            let state =
                |c: &Cpu<NoDevices>| (c.x[0], c.x[30], c.pc, c.sys.esr, c.sys.elr, c.sys.far);
            assert_eq!(state(&jit), state(&interp), "{what}, round {round}");
        }
        assert!(jit.jit.block_runs > 0, "{what}: nothing ran compiled");
        if target & 3 != 0 {
            // The architecture's answer, not only the interpreter's: a PC alignment fault
            assert_eq!(
                interp.sys.esr >> 26,
                crate::cpu::ec::PC_ALIGN as u64,
                "{what}"
            );
            assert_eq!((interp.sys.elr, interp.sys.far), (target, target), "{what}");
        }
    }
}

/// Writes to a page with code: data next to it keeps the compiled code (and the decoded
/// instructions); a write to an instruction it was compiled from replaces it, interpreted and
/// compiled alike.
#[test]
fn writes_next_to_code_keep_it_and_writes_to_it_replace_it() {
    const PATCH: u64 = 0x3000;
    // PATCH: str w3, [x4]; str x0, [x2]; b EXIT
    let patch = [
        0xb900_0083u32,
        0xf900_0040,
        0x1400_0000 | ((EXIT - PATCH - 8) / 4) as u32,
    ];
    let setup = |c: &mut Cpu<NoDevices>| {
        for (i, w) in patch.iter().enumerate() {
            c.mem.write(PATCH as usize + 4 * i, &w.to_le_bytes());
        }
    };
    let run = |c: &mut Cpu<NoDevices>, pc: u64, n: u64| {
        c.pc = pc;
        c.jit_entry = true;
        c.run(n);
        c.wait_native_jit();
    };
    let Some(mut jit) = at_block(1) else { return };
    let mut interp = at_block(1).unwrap();
    interp.jit.enabled = false; // the interpreter alone, with its decoded pages
    setup(&mut jit);
    setup(&mut interp);
    for c in [&mut jit, &mut interp] {
        for _ in 0..3 {
            run(c, CODE, 2);
        }
        assert_eq!(c.x[0], 3);
        // PATCH compiled too, writing elsewhere (a page without code)
        (c.x[2], c.x[3], c.x[4]) = (0x8008, 0, 0x8000);
        run(c, PATCH, 3);
        run(c, PATCH, 3);
    }
    let compiled = jit.jit.compiled;
    let invalidated = jit.jit.page_invalidations;

    // Data next to the block, on its page: the code stays (no new compile, no invalidation)
    for c in [&mut jit, &mut interp] {
        (c.x[2], c.x[3], c.x[4]) = (CODE + 0x808, 0xdead_beef, CODE + 0x800);
        run(c, PATCH, 3);
        run(c, CODE, 2);
        assert_eq!(c.x[0], 4);
        assert_eq!(c.mem.load(CODE as usize + 0x808, 8), 3);
    }
    assert_eq!(
        jit.jit.compiled, compiled,
        "recompiled after a write next to the code"
    );
    assert_eq!(jit.jit.page_invalidations, invalidated);
    assert!(jit.jit.code_writes_kept > 0);

    // The block's own instruction rewritten: add x0, x0, #2 runs from now on
    for c in [&mut jit, &mut interp] {
        (c.x[3], c.x[4]) = (0x9100_0800, CODE);
        run(c, PATCH, 3);
        for _ in 0..3 {
            run(c, CODE, 2);
        }
        assert_eq!(c.x[0], 10, "the new instruction ran");
    }
    assert!(jit.jit.page_invalidations > invalidated);
}

/// A CPU at the start of a two-instruction block (`add x0, x0, #1; b EXIT`) at CODE.
fn at_block(threshold: u32) -> Option<Cpu<NoDevices>> {
    let mut c = Cpu::new(RAM, NoDevices);
    if !c.enable_native_jit() {
        return None; // no Cranelift backend for this host
    }
    c.jit.threshold = threshold;
    let words = [0x9100_0400u32, 0x1400_0000 | ((EXIT - CODE - 4) / 4) as u32];
    for (i, w) in words.iter().enumerate() {
        let a = CODE as usize + 4 * i;
        c.mem.write(a, &w.to_le_bytes());
    }
    c.mem.write(EXIT as usize, &0x1400_0000u32.to_le_bytes());
    (c.el, c.spsel, c.daif) = (1, true, 0x3c0);
    Some(c)
}

/// Enter the block (as a branch does) and run it.
fn enter(c: &mut Cpu<NoDevices>) {
    c.pc = CODE;
    c.jit_entry = true;
    c.run(2);
}

/// A profile of what one machine compiled and ran makes another compile it before its first
/// run, unless the page holds other code now.
#[test]
fn a_profile_prewarms_the_same_code_and_skips_changed_pages() {
    let Some(mut a) = at_block(1) else { return };
    for _ in 0..3 {
        enter(&mut a);
        a.wait_native_jit();
    }
    let profile = a.jit_profile();
    let page = profile
        .iter()
        .find(|p| p.key >> 14 == CODE >> 12)
        .unwrap_or_else(|| panic!("CODE's page is not in {profile:?}"));
    assert!(page.entries.contains(&0), "{page:?}");

    // A profile survives its encoding
    let mut b = at_block(u32::MAX).unwrap();
    let back = crate::jit::profile::read(&crate::jit::profile::write(&profile)).unwrap();
    assert_eq!(b.jit_prewarm(&back), profile.len());
    b.wait_native_jit();
    let runs = b.jit.block_runs;
    enter(&mut b);
    assert_eq!(
        b.jit.block_runs,
        runs + 1,
        "the first run was not compiled code"
    );
    assert_eq!(b.x[0], 1);
    assert_eq!(b.pc, EXIT);

    // Other code on the page: nothing compiled for it, and it still runs (interpreted)
    let mut c = at_block(u32::MAX).unwrap();
    c.mem.write(CODE as usize, &0x9100_0800u32.to_le_bytes());
    assert_eq!(c.jit_prewarm(&profile), profile.len() - 1);
    c.wait_native_jit();
    let runs = c.jit.block_runs;
    enter(&mut c);
    assert_eq!(c.jit.block_runs, runs);
    assert_eq!(c.x[0], 2);
}

/// Compiling on the caller's thread: a profile is compiled before `jit_prewarm` returns, with
/// no thread; once compiling moves to a thread, that code still runs and new code goes into a
/// module of the thread's own.
#[test]
fn a_jit_that_compiles_here_has_its_code_at_once_and_keeps_it_on_a_thread() {
    let Some(mut a) = at_block(1) else { return };
    for _ in 0..3 {
        enter(&mut a);
        a.wait_native_jit();
    }
    let profile = a.jit_profile();

    let mut b = at_block(u32::MAX).unwrap();
    assert!(b.enable_native_jit_here());
    b.jit.threshold = u32::MAX;
    let n = b.jit.native.as_ref().unwrap();
    assert!(n.compiles_here() && n.thread.is_none());
    assert_eq!(b.jit_prewarm(&profile), profile.len());
    // No waiting: the code is there
    assert!(b.jit.native.as_ref().unwrap().has_done());
    let here_bytes = b.native_jit_code_bytes();
    assert!(here_bytes > 0);
    let runs = b.jit.block_runs;
    enter(&mut b);
    assert_eq!(b.jit.block_runs, runs + 1, "the profile's code did not run");
    assert_eq!(b.x[0], 1);

    // On a thread now: the code compiled here still runs
    assert!(b.enable_native_jit());
    let n = b.jit.native.as_ref().unwrap();
    assert!(!n.compiles_here() && n.thread.is_some() && n.kept.is_some());
    assert_eq!(b.native_jit_code_bytes(), 0, "a module of its own");
    enter(&mut b);
    assert_eq!(b.jit.block_runs, runs + 2);
    assert_eq!(b.x[0], 2);
    // and new code compiles into the thread's module
    const OTHER: u64 = 0x8000;
    let back = ((EXIT as i64 - (OTHER + 4) as i64) / 4) as u32 & 0x3ff_ffff;
    let words = [0x9100_0c00u32, 0x1400_0000 | back];
    for (i, w) in words.iter().enumerate() {
        b.mem.write(OTHER as usize + 4 * i, &w.to_le_bytes());
    }
    b.jit.threshold = 1;
    for _ in 0..3 {
        b.pc = OTHER;
        b.jit_entry = true;
        b.run(2);
        b.wait_native_jit();
    }
    let runs = b.jit.block_runs;
    b.pc = OTHER;
    b.jit_entry = true;
    b.run(2);
    assert_eq!(b.jit.block_runs, runs + 1, "new code was not compiled");
    assert_eq!(b.x[0], 2 + 4 * 3);
    assert!(b.native_jit_code_bytes() > 0);
    enter(&mut b);
    assert_eq!(b.jit.block_runs, runs + 2, "the kept code went");
}

/// Groups that are translated in full: what real workloads (bench/sites.exs) used to hand to
/// the interpreter. Compiled, none of them may reach it.
const STRICT: &[(u32, u32)] = &[
    (0x7fff_fc00, 0x5ac0_0000), // RBIT
    (0x7fff_fc00, 0x5ac0_1400), // CLS
    (0x7fff_f800, 0x5ac0_1000), // CLZ, CLS
    (0x1fe0_fc00, 0x1a00_0000), // ADC, ADCS, SBC, SBCS (NGC)
    (0xffff_ffe0, 0xd50b_7420), // DC ZVA
    (0xffff_ffe0, 0xd53b_00e0), // MRS Xt, DCZID_EL0
    (0x9ff8_9c00, 0x0f00_0400), // MOVI, MVNI (vector, immediate): 32-bit shifted (cmode 0xx0)
    (0x9ff8_9c00, 0x0f00_1400), // ORR, BIC (vector, immediate): 32-bit shifted (cmode 0xx1)
    (0x9ff8_dc00, 0x0f00_8400), // MOVI, MVNI: 16-bit shifted (cmode 10x0)
    (0x9ff8_dc00, 0x0f00_9400), // ORR, BIC: 16-bit shifted (cmode 10x1)
    (0x9ff8_ec00, 0x0f00_c400), // ... 32-bit shifting ones
    (0xbff8_fc00, 0x0f00_e400), // MOVI 8-bit; MOVI 64-bit (U=1)
    (0xfff8_fc00, 0x4f00_f400), // FMOV (vector, immediate) .4S
    (0xfff8_fc00, 0x6f00_f400), // FMOV (vector, immediate) .2D
    (0x9f60_f400, 0x0e20_6400), // SMAX, UMAX, SMIN, UMIN (8- and 16-bit lanes)
    (0x9f60_f400, 0x0ea0_6400), // ... 32-bit lanes
    (0x9f60_f400, 0x0e20_a400), // SMAXP, UMAXP, SMINP, UMINP (8- and 16-bit lanes)
    (0x9f60_f400, 0x0ea0_a400), // ... 32-bit lanes
    (0xbf00_0000, 0x1800_0000), // LDR Wt, Xt (literal)
    (0xff00_0000, 0x9800_0000), // LDRSW (literal)
    (0xff00_0000, 0xd800_0000), // PRFM (literal)
    (0xff00_0000, 0x1c00_0000), // LDR St (literal)
    (0xff00_0000, 0x5c00_0000), // LDR Dt (literal)
    (0xff00_0000, 0x9c00_0000), // LDR Qt (literal)
    (0xffe1_fc00, 0x0e01_3c00), // UMOV Wd, Vn.B[i]
    (0xffe3_fc00, 0x0e02_3c00), // UMOV Wd, Vn.H[i]
    (0xffe7_fc00, 0x0e04_3c00), // UMOV Wd, Vn.S[i]
    (0xffef_fc00, 0x4e08_3c00), // UMOV Xd, Vn.D[i]
    (0xbfe1_fc00, 0x0e01_2c00), // SMOV Wd/Xd, Vn.B[i]
    (0xbfe3_fc00, 0x0e02_2c00), // SMOV Wd/Xd, Vn.H[i]
    (0xffe7_fc00, 0x4e04_2c00), // SMOV Xd, Vn.S[i]
    (0x9ff8_fc00, 0x0f08_a400), // SSHLL / USHLL (8-bit lanes)
    (0x9ff0_fc00, 0x0f10_a400), // ... 16-bit lanes
    (0x9fe0_fc00, 0x0f20_a400), // ... 32-bit lanes
];
/// The system instructions among them (MRS DCZID_EL0 too).
const STRICT_SYSTEM: &[(u32, u32)] = &[(0xffff_ffe0, 0xd50b_7420), (0xffff_ffe0, 0xd53b_00e0)];

#[test]
fn translated_groups_never_reach_the_interpreter() {
    fuzz_with(
        0x6a09_e667_f3bc_c908,
        4000,
        STRICT,
        false,
        true,
        STRICT_SYSTEM,
    );
}

/// The same, for the narrow groups.
#[test]
fn compiled_narrow_groups_do_what_the_interpreter_does() {
    fuzz(0x51_7cc1_b727_220a, 6000, NARROW, false);
}

/// The same, for scalar floating point alone, with operands that make exact results and the
/// edges common: results and FPSR must match.
#[test]
fn compiled_fp_does_what_the_interpreter_does() {
    fuzz(0x9e37_79b9_7f4a_7c15, 8000, &BODY[BODY.len() - 10..], true);
}

fn fuzz(seed: u64, cases: usize, body: &[(u32, u32)], fp_bias: bool) {
    fuzz_with(seed, cases, body, fp_bias, false, &[])
}

/// `strict`: every instruction `body` makes is translated, so a compiled block of them never
/// hands one to the interpreter (results alone can't tell: the interpreter's are the same).
fn fuzz_with(
    seed: u64,
    cases: usize,
    body: &[(u32, u32)],
    fp_bias: bool,
    strict: bool,
    system: &[(u32, u32)],
) {
    let mut rng = Rng(seed);
    let mut interp = Cpu::new(RAM, NoDevices);
    let mut jit = Cpu::new(RAM, NoDevices);
    if !jit.enable_native_jit() {
        return; // no Cranelift backend for this host
    }
    jit.jit.threshold = 1;
    let mut compared = 0;
    for case in 0..cases {
        let k = 1 + rng.below(12) as usize;
        let start_pc = CODE + rng.below((1024 - k - 1) as u64) * 4;
        let mut mem = vec![0u8; RAM];
        for b in mem[0x10000..0x30000].iter_mut() {
            *b = rng.next() as u8;
        }
        let mut words: Vec<u32> = (0..k)
            .map(|_| loop {
                let (mask, value) = body[rng.below(body.len() as u64) as usize];
                let i = (rng.next() as u32 & !mask) | value;
                // (no branches or system instructions, but those `system` lets through)
                if (i >> 26) & 7 != 0b101 || system.iter().any(|&(m, v)| i & m == v) {
                    break i;
                }
            })
            .collect();
        let branch_at = start_pc + 4 * k as u64;
        words.push(0x1400_0000 | (((EXIT as i64 - branch_at as i64) / 4) as u32 & 0x3ff_ffff));
        for (i, w) in words.iter().enumerate() {
            let a = start_pc as usize + 4 * i;
            mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
        }
        mem[EXIT as usize..EXIT as usize + 4].copy_from_slice(&0x1400_0000u32.to_le_bytes());
        let regs: [u64; 31] = std::array::from_fn(|_| match rng.below(4) {
            0 | 1 => 0x10000 + rng.below(0x20000),
            2 => rng.below(64),
            _ => rng.next(),
        });
        let el = if rng.below(5) == 0 { 0 } else { 1 };
        let (sp0, sp1) = (0x10000 + rng.below(0x20000), 0x10000 + rng.below(0x20000));
        let flags = rng.next();
        // SIMD&FP mostly usable (FPEN = 3), else any FPEN, which traps some ELs
        // (strictly translated code must not be trapped either: a trap is the interpreter's)
        let fpen = if !strict && rng.below(4) == 0 { rng.below(4) } else { 3 };
        let vregs: [u128; 32] = std::array::from_fn(|_| {
            let hi = (rng.next() as u128) << 64;
            match rng.below(if fp_bias { 6 } else { 3 }) {
                0 => hi | rng.next() as u128,
                3 | 4 => {
                    hi | (rng.next() as u128 & !0xffff_ffff) | fp_small(&mut rng, false) as u128
                },
                5 => hi | fp_small(&mut rng, true) as u128,
                1 => {
                    hi | (rng.next() as u128 & !0xffff_ffff) | fp_value(&mut rng, false) as u128
                },
                _ => hi | fp_value(&mut rng, true) as u128,
            }
        });
        // FPCR mostly as Linux runs (all clear), else any rounding mode, FZ, DN or AHP
        let fpcr = if rng.below(4) == 0 { rng.next() & 0x07c0_0000 } else { 0 };
        // Exception flags mostly clear, so that one wrongly raised shows
        let fpsr = if rng.below(4) == 0 { rng.next() & 0x9f } else { 0 };
        let load = |c: &mut Cpu<NoDevices>, drop_code: bool| {
            c.mem.write(0, &mem);
            if drop_code {
                c.jit.invalidate_page((CODE >> 12) as usize);
            }
            c.x[..31].copy_from_slice(&regs);
            c.pc = start_pc;
            (c.sp_el0, c.sp_el1) = (sp0, sp1);
            (c.n, c.z, c.c, c.v) = (
                flags & 1 != 0,
                flags & 2 != 0,
                flags & 4 != 0,
                flags & 8 != 0,
            );
            c.el = el;
            c.spsel = el == 1;
            c.daif = 0x3c0;
            (c.sys.esr, c.sys.far, c.sys.elr, c.sys.spsr) = (0, 0, 0, 0);
            c.vreg = vregs;
            c.sys.cpacr = fpen << 20;
            (c.sys.fpcr, c.sys.fpsr) = (fpcr, fpsr);
            c.set_excl(None);
            // Entered through a branch, as a block is
            c.jit_entry = true;
        };
        load(&mut interp, true);
        let block = start_pc..branch_at + 4;
        for _ in 0..=k {
            let _ = interp.step();
            if !block.contains(&interp.pc) {
                break;
            }
        }
        for warm in 0..3 {
            load(&mut jit, warm == 0);
            jit.run(1);
            jit.wait_native_jit();
        }
        load(&mut jit, false);
        let runs = jit.jit.block_runs;
        let helped = jit.jit.helper_interps;
        jit.run(1);
        if jit.jit.block_runs == runs {
            continue; // e.g. a block that stores into its own page is never kept compiled
        }
        assert!(
            !strict || jit.jit.helper_interps == helped,
            "case {case}: compiled code handed an instruction to the interpreter: {words:08x?}"
        );
        compared += 1;
        assert_eq!(
            state(&jit),
            state(&interp),
            "case {case}: block {words:08x?}"
        );
        assert!(
            jit.mem.to_vec() == interp.mem.to_vec(),
            "case {case}: RAM differs, block {words:08x?}"
        );
    }
    assert!(
        compared > cases * 95 / 100,
        "only {compared} blocks ran as native code"
    );
    assert_eq!(
        jit.native_jit_failures(),
        0,
        "blocks `gen` made that did not compile"
    );
}
