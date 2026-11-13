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
    c.mem.write(addr, &insn.to_le_bytes());
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
fn a_limit_in_bytes_caps_the_slots_a_cache_allocates() {
    let mut c = cpu();
    c.jit.pre.set_limit(4 * SLOT_OPS * std::mem::size_of::<Op>());
    assert_eq!(c.jit.pre.max_slots, 4);
    for p in 0..10 {
        put(&mut c, 0x1000 * (p + 1), ADD_X0_1);
        run_at(&mut c, 0x1000 * (p as u64 + 1));
    }
    assert_eq!(c.x[0], 10);
    assert_eq!(c.jit.pre.bytes().0, 4);
    // Never below two slots: the page running and the one it jumps to
    c.jit.pre.set_limit(0);
    assert_eq!(c.jit.pre.max_slots, 2);
}

/// The bytes of the `PAGE_INSNS` ops of a slot, and whether its last op is the `End`.
fn slot_ops(c: &Cache, slot: u32) -> (&[u8], bool) {
    let p = c.slots[slot as usize];
    let n = PAGE_INSNS * std::mem::size_of::<Op>();
    // SAFETY: the slot is the cache's, `SLOT_OPS` ops long, and nothing writes it while the slice lives.
    unsafe {
        (
            std::slice::from_raw_parts(p as *const u8, n),
            matches!((*p.add(PAGE_INSNS)).k, K::End),
        )
    }
}

#[test]
fn a_recycled_slot_reads_all_miss_and_leaves_its_neighbours_alone() {
    // A slot's stride is not a multiple of the host page, so slots start at different offsets in their
    // pages: the head and tail fragments and the whole pages between them are all exercised.
    let mut c = Cache::new(64 << 20);
    c.max_slots = 8;
    let n = PAGE_INSNS * std::mem::size_of::<Op>();
    // Every op of every page "decoded": non-zero bytes (never read as an `Op`).
    let fill = |c: &Cache, slot: u32| {
        // SAFETY: the `PAGE_INSNS` ops of a slot of the cache, which nothing else reads now.
        unsafe { std::ptr::write_bytes(c.slots[slot as usize] as *mut u8, 0xaa, n) }
    };
    for page in 0..8 {
        let slot = c.alloc(page);
        assert_eq!(slot, page as u32);
        fill(&c, slot);
    }
    for round in 0..2 {
        for victim in 0..8u32 {
            // Dropped, then handed to another page: the slot comes back from the free list.
            c.drop_page(c.slot_page[victim as usize] as usize);
            assert_eq!(c.alloc(100 + 8 * round + victim as usize), victim);
            let (ops, end) = slot_ops(&c, victim);
            assert!(ops.iter().all(|&b| b == 0), "slot {victim} not all Miss");
            assert!(end);
            for other in (0..8u32).filter(|&o| o != victim) {
                let (ops, end) = slot_ops(&c, other);
                assert!(ops.iter().all(|&b| b == 0xaa), "slot {other} touched by {victim}");
                assert!(end);
            }
            // Decoded into again, so the next recycle has something to clear.
            fill(&c, victim);
        }
    }
}

#[test]
fn more_code_pages_than_are_kept_still_run_correctly() {
    let mut c = cpu();
    let flushes = c.jit.pre.flushes;
    c.jit.pre.max_slots = 8;
    let pages = 8 + 10;
    for p in 0..pages {
        put(&mut c, 0x1000 * (p + 1), ADD_X0_1);
    }
    for round in 1..=2 {
        for p in 0..pages {
            run_at(&mut c, 0x1000 * (p as u64 + 1));
        }
        assert_eq!(c.x[0], (round * pages) as u64);
    }
    // The slots of the pages used the longest ago were given to the others, and nothing was cleared.
    assert!(c.jit.pre.evictions >= 10);
    assert_eq!(c.jit.pre.flushes, flushes);
}

const NOP: u32 = 0xd503_201f;
const WFI: u32 = 0xd503_207f;
const SVC_0: u32 = 0xd400_0001;
const ADR_X1_16: u32 = 0x1000_0081; // adr x1, .+16
const LDR_X2_LIT: u32 = 0x5800_0102; // ldr x2, .+32

#[test]
fn a_run_over_decoded_pages_counts_every_instruction_and_stops_at_the_budget() {
    let mut c = cpu();
    // 1024 instructions fill the page at 0x1000, and the code goes on in the next page.
    for i in 0..1124 {
        put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
    }
    // The second round runs from entries decoded in the first.
    for _ in 0..2 {
        c.pc = 0x1000;
        c.x[0] = 0;
        let before = c.insn_count;
        assert!(matches!(c.run(1100), crate::cpu::Exit::Budget));
        assert_eq!(c.x[0], 1100);
        assert_eq!(c.insn_count - before, 1100);
        assert_eq!(c.pc, 0x1000 + 4 * 1100);
    }
}

#[test]
fn a_store_into_the_page_being_run_takes_effect_at_the_next_instruction() {
    let mut c = cpu();
    // str w2, [x3]; nop; movz x0, #1: the store rewrites the movz.
    put(&mut c, 0x1000, STR_W2_X3);
    put(&mut c, 0x1004, NOP);
    put(&mut c, 0x1008, MOVZ_X0 | 1 << 5);
    c.x[3] = 0x1008;
    for value in 1..4u32 {
        c.pc = 0x1000;
        c.x[2] = (MOVZ_X0 | value << 5) as u64;
        c.run(3);
        assert_eq!(c.x[0], value as u64);
    }
    assert_eq!(c.insn_count, 9);
}

#[test]
fn instructions_that_read_the_pc_see_it_in_the_middle_of_a_run() {
    let mut c = cpu();
    put(&mut c, 0x1000, ADD_X0_1);
    put(&mut c, 0x1004, ADR_X1_16);
    put(&mut c, 0x1008, LDR_X2_LIT);
    c.mem.write(0x1028, &0x1234_5678_9abc_def0u64.to_le_bytes());
    for round in 1..=2u64 {
        c.pc = 0x1000;
        c.x[1] = 0;
        c.x[2] = 0;
        c.run(3);
        assert_eq!(c.x[0], round);
        assert_eq!(c.x[1], 0x1014);
        assert_eq!(c.x[2], 0x1234_5678_9abc_def0);
        assert_eq!(c.pc, 0x100c);
    }
}

#[test]
fn an_exception_in_the_middle_of_a_run_returns_to_the_right_instruction() {
    let mut c = cpu();
    c.sys.vbar = 0x4000;
    put(&mut c, 0x1000, ADD_X0_1);
    put(&mut c, 0x1004, ADD_X0_1);
    put(&mut c, 0x1008, SVC_0);
    put(&mut c, 0x100c, ADD_X0_1);
    for round in 1..=2u64 {
        c.pc = 0x1000;
        c.x[0] = 0;
        c.run(3);
        assert_eq!(c.x[0], 2);
        assert_eq!(c.insn_count, 3 * round);
        assert_eq!(c.sys.elr, 0x100c);
        assert_eq!(c.sys.esr >> 26, 0x15);
        assert!((0x4000..0x4800).contains(&c.pc));
        // The guest returned from the handler, as far as this test goes.
        c.el = 1;
        c.spsel = true;
    }
}

#[test]
fn waiting_for_an_interrupt_in_the_middle_of_a_run_stops_the_run() {
    let mut c = cpu();
    put(&mut c, 0x1000, ADD_X0_1);
    put(&mut c, 0x1004, WFI);
    put(&mut c, 0x1008, ADD_X0_1);
    for round in 1..=2u64 {
        c.pc = 0x1000;
        c.x[0] = 0;
        assert!(matches!(c.run(10), crate::cpu::Exit::Idle { .. }));
        assert_eq!(c.x[0], 1);
        assert_eq!(c.insn_count, 2 * round);
        assert_eq!(c.pc, 0x1008);
    }
}
const SUB_X0_1: u32 = 0xd100_0400; // sub x0, x0, #1
const CBNZ_X0_BACK: u32 = 0xb5ff_ffe0; // cbnz x0, .-4
const B_BACK: u32 = 0x17ff_ffff; // b .-4

#[test]
fn a_block_is_decoded_once_and_run_from_its_ops_afterwards() {
    let mut c = cpu();
    for i in 0..100 {
        put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
    }
    c.pc = 0x1000;
    c.run(100);
    assert_eq!(c.x[0], 100);
    // A block of 64 instructions, and a second that ends at the word after the last ADD (a zero word,
    // which `Gen` runs), which the run did not reach.
    let (blocks, insns) = (c.jit.pre.blocks, c.jit.pre.insns);
    assert_eq!(blocks, 2);
    assert_eq!(insns, 64 + 37);
    c.pc = 0x1000;
    c.run(100);
    assert_eq!(c.x[0], 200);
    assert_eq!((c.jit.pre.blocks, c.jit.pre.insns), (blocks, insns));
    assert_eq!(c.jit.pre.decoded, 1);
}

#[test]
fn a_run_of_any_length_ends_exactly_at_its_budget() {
    let mut c = cpu();
    for i in 0..100 {
        put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
    }
    let mut counted = 0;
    for n in 1..=100u64 {
        c.pc = 0x1000;
        c.x[0] = 0;
        c.run(n);
        counted += n;
        assert_eq!(c.x[0], n);
        assert_eq!(c.pc, 0x1000 + 4 * n);
        assert_eq!(c.insn_count, counted);
    }
}

#[test]
fn a_loop_in_the_page_runs_to_the_budget_and_counts_its_instructions() {
    for body in [[SUB_X0_1, CBNZ_X0_BACK], [ADD_X0_1, B_BACK]] {
        let mut c = cpu();
        put(&mut c, 0x1000, body[0]);
        put(&mut c, 0x1004, body[1]);
        for round in 1..=3u64 {
            c.pc = 0x1000;
            c.x[0] = 1000;
            let before = c.insn_count;
            c.run(1001);
            assert_eq!(c.insn_count - before, 1001, "round {round}");
            // `sub; cbnz` is back at the sub after an even number of instructions, `add; b` too.
            assert_eq!(c.pc, 0x1004, "round {round}");
        }
    }
}

#[test]
fn an_exception_after_many_instructions_returns_to_the_right_one() {
    let mut c = cpu();
    c.sys.vbar = 0x4000;
    for i in 0..100 {
        put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
    }
    put(&mut c, 0x1000 + 4 * 100, SVC_0);
    put(&mut c, 0x1000 + 4 * 101, ADD_X0_1);
    for round in 1..=2u64 {
        c.pc = 0x1000;
        c.x[0] = 0;
        c.run(101);
        assert_eq!(c.x[0], 100);
        assert_eq!(c.insn_count, 101 * round);
        assert_eq!(c.sys.elr, 0x1000 + 4 * 101);
        assert_eq!(c.sys.esr >> 26, 0x15);
        c.el = 1;
        c.spsel = true;
    }
}

#[test]
fn a_load_that_faults_in_a_run_returns_to_itself() {
    let mut c = cpu();
    c.sys.vbar = 0x4000;
    // ldr x1, [x2] with x2 outside RAM.
    put(&mut c, 0x1000, ADD_X0_1);
    put(&mut c, 0x1004, 0xf940_0041);
    put(&mut c, 0x1008, ADD_X0_1);
    c.x[2] = 0x4000_0000_0000;
    c.pc = 0x1000;
    c.run(2);
    assert_eq!(c.x[0], 1);
    assert_eq!(c.sys.elr, 0x1004);
    // A data abort, from the current or a lower exception level.
    assert_eq!((c.sys.esr >> 26) | 1, 0x25);
}

#[test]
fn the_flags_a_compare_leaves_are_seen_by_the_instructions_after_it() {
    let mut c = cpu();
    // subs xzr, x0, x1; b.lo .+8; mov x2, #1; mov x3, #2
    put(&mut c, 0x1000, 0xeb01_001f);
    put(&mut c, 0x1004, 0x5400_0043);
    put(&mut c, 0x1008, 0xd280_0022);
    put(&mut c, 0x100c, 0xd280_0043);
    for (x0, x1, lower) in [
        (1u64, 2u64, true),
        (2, 1, false),
        (u64::MAX, 1, false),
        (0, u64::MAX, true),
    ] {
        for _ in 0..2 {
            c.pc = 0x1000;
            c.x[0] = x0;
            c.x[1] = x1;
            c.x[2] = 0;
            c.x[3] = 0;
            c.run(if lower { 3 } else { 4 });
            assert_eq!(
                (c.x[2], c.x[3]),
                if lower { (0, 2) } else { (1, 2) },
                "{x0} {x1}"
            );
        }
    }
}
