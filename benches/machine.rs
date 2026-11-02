//! Host-side costs, timed with criterion: what one guest instruction costs interpreted and
//! compiled, what compiling a page costs, and what saving and restoring a machine costs.
//!
//!   cargo bench --bench machine                          # all of them
//!   cargo bench --bench machine -- --save-baseline main  # keep the results as "main"
//!   cargo bench --bench machine -- --baseline main       # compare with them
//!
//! The guest-level picture (phases of a real program, the JIT's statistics) is bench/run.exs;
//! see docs/profiling.md.

use std::hint::black_box;

use armless::board::Host;
#[cfg(feature = "native-jit")]
use armless::jit::profile::{self, Page};
use armless::runner;
use armless::vm::Config;
use armless::Cpu;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};

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

const RAM: usize = 16 << 20;
const CODE: u64 = 0x1000;
const DATA: u64 = 0x8000;
/// Instructions each measured iteration runs.
const BUDGET: u64 = 1_000_000;

/// A loop of ALU work, a load and a store, forever (7 instructions a round):
///
/// ```text
///         mov  x1, #0
/// loop:   add  x0, x0, x1
///         eor  x2, x2, x0
///         ldr  x3, [x4]
///         str  x2, [x4, #8]
///         add  x1, x1, #1
///         cmp  x1, x5
///         b.ne loop
/// ```
const LOOP: [u32; 8] = [
    0xd280_0001,
    0x8b01_0000,
    0xca00_0042,
    0xf940_0083,
    0xf900_0482,
    0x9100_0421,
    0xeb05_003f,
    0x54ff_ff41,
];

fn cpu() -> Cpu<NoDevices> {
    let mut c = Cpu::new(RAM, NoDevices);
    for (i, w) in LOOP.iter().enumerate() {
        c.mem.write(CODE as usize + 4 * i, &w.to_le_bytes());
    }
    (c.el, c.spsel, c.daif) = (1, true, 0x3c0);
    c.x[4] = DATA;
    c.x[5] = u64::MAX;
    c.pc = CODE;
    c
}

/// The loop's page, compiled with its two entries (the start and the loop).
#[cfg(feature = "native-jit")]
fn profile(c: &Cpu<NoDevices>) -> Page {
    let mut page = [0u8; 0x1000];
    c.mem.read(CODE as usize, &mut page);
    Page {
        key: (CODE << 2) | 0b11,
        hash: profile::hash(&page),
        runs: 1,
        entries: vec![0, 4],
    }
}

fn execution(c: &mut Criterion) {
    let mut g = c.benchmark_group("execute");
    g.throughput(Throughput::Elements(BUDGET));

    let mut interp = cpu();
    g.bench_function("interpreter", |b| b.iter(|| black_box(interp.run(BUDGET))));

    #[cfg(feature = "native-jit")]
    let mut jit = cpu();
    #[cfg(feature = "native-jit")]
    if jit.enable_native_jit() {
        let page = profile(&jit);
        jit.jit_prewarm(&[page]);
        jit.wait_native_jit();
        jit.run(BUDGET);
        g.bench_function("native jit", |b| b.iter(|| black_box(jit.run(BUDGET))));
    }
    g.finish();
}

/// What compiling a page costs (nothing to measure without the native JIT).
#[cfg(not(feature = "native-jit"))]
fn compile(_: &mut Criterion) {}

#[cfg(feature = "native-jit")]
fn compile(c: &mut Criterion) {
    if !cpu().enable_native_jit() {
        return; // no Cranelift backend for this host
    }
    c.bench_function("native jit: compile a page", |b| {
        b.iter_batched(
            || {
                let mut c = cpu();
                c.enable_native_jit();
                let page = profile(&c);
                (c, page)
            },
            |(mut c, page)| {
                c.jit_prewarm(&[page]);
                c.wait_native_jit();
                c
            },
            criterion::BatchSize::PerIteration,
        )
    });
}

fn snapshot(c: &mut Criterion) {
    let mut g = c.benchmark_group("snapshot");
    let (mut vm, _clock) = runner::new_vm(Config {
        memory_size: 64 << 20,
        cpus: 1,
        ..Default::default()
    })
    .unwrap();
    // 256 pages the guest wrote (1 MiB), spread over RAM
    for p in 0..256usize {
        vm.cpus[0]
            .mem
            .write(p * (64 << 20) / 256, &(p as u64 + 1).to_le_bytes());
    }
    let state = vm.save_state();
    g.throughput(Throughput::Bytes(64 << 20));
    g.bench_function("save 64 MiB", |b| b.iter(|| black_box(vm.save_state())));
    g.bench_function("restore 64 MiB", |b| {
        b.iter(|| vm.load_state(black_box(&state)).unwrap())
    });
    g.finish();
}

criterion_group!(benches, execution, compile, snapshot);
criterion_main!(benches);
