//! What system calls cost the host, timed with criterion: each case is a program (`Prog`, as the
//! tests write them) that makes its calls in a loop, run as many times as criterion asks, and timed
//! from its start to its exit. The CPUs are the tests' (ARMLESS_TEST_CPUS, else one).
//!
//!   cargo bench -p armless-user --bench syscalls
//!   ARMLESS_TEST_CPUS=4 cargo bench -p armless-user --bench syscalls -- pipe

#[path = "../tests/prog/mod.rs"]
mod prog;

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use prog::*;

/// mmap(len) of anonymous memory, its address kept at `off`
fn anon(p: &mut Prog, len: u64, off: u32) {
    p.sys(222, &[I(0), I(len), I(3), I(0x22), I(u64::MAX), I(0)])
        .save(off);
}

/// `body` `n` times (x19 counts)
fn repeat(p: &mut Prog, n: u64, body: impl Fn(&mut Prog)) {
    p.li(19, n);
    let top = p.code.len();
    body(p);
    p.code.push(0xf100_0673); // subs x19, x19, #1
    let off = top as i64 - p.code.len() as i64;
    p.code.push(0x5400_0001 | (((off as u32) & 0x7ffff) << 5)); // b.ne top
}

/// Times the program `make` writes for a loop of `n` rounds, from its start to its exit.
fn timed(make: &dyn Fn(&mut Prog, u64), n: u64) -> Duration {
    let mut p = Prog::default();
    make(&mut p, n);
    let mut vm = machine();
    let mut proc_ = start(&mut vm, p);
    let t = Instant::now();
    let (_, status) = run(&mut proc_, &mut vm);
    let took = t.elapsed();
    ok(&status);
    took
}

fn case(c: &mut Criterion, name: &str, make: impl Fn(&mut Prog, u64)) {
    c.bench_function(name, |b| b.iter_custom(|n| timed(&make, n)));
}

fn syscalls(c: &mut Criterion) {
    case(c, "getpid", |p, n| {
        repeat(p, n, |p| {
            p.sys(172, &[]);
        })
    });
    case(c, "open, read 64 bytes, close /proc/self/maps", |p, n| {
        p.put(0x200, b"/proc/self/maps\0");
        repeat(p, n, |p| {
            p.sys(56, &[I(-100i64 as u64), D(0x200), I(0)]).save(0x10);
            p.sys(63, &[X(0x10), D(0x400), I(64)]);
            p.sys(57, &[X(0x10)]);
        });
    });
    for (name, path, nr) in [
        ("write 64 KiB to /dev/null", "/dev/null\0", 64u64),
        ("read 64 KiB of /dev/zero", "/dev/zero\0", 63),
    ] {
        case(c, name, |p, n| {
            p.put(0x200, path.as_bytes());
            p.sys(56, &[I(-100i64 as u64), D(0x200), I(2)]).save(0x10);
            anon(p, 64 << 10, 8);
            repeat(p, n, |p| {
                p.sys(nr, &[X(0x10), X(8), I(64 << 10)]);
            });
        });
    }
}

/// A call that changes the address space, then a store to each of 32 pages (which a TLB flushed
/// whole has to translate again)
fn tlb(c: &mut Criterion) {
    for (name, nr, args) in [
        ("getpid, 32 page stores", 172u64, vec![]),
        (
            "madvise DONTNEED 1 page, 32 page stores",
            233,
            vec![X(0x18), I(4096), I(4)],
        ),
        (
            "mprotect 1 page, 32 page stores",
            226,
            vec![X(0x18), I(4096), I(3)],
        ),
    ] {
        case(c, name, |p, n| {
            anon(p, 8 << 20, 8);
            anon(p, 4096, 0x18);
            p.arg(20, X(8));
            repeat(p, n, |p| {
                p.sys(nr, &args);
                for k in 0..32u32 {
                    p.code.push(0x9140_0000 | (k * 16) << 10 | 20 << 5 | 21); // add x21, x20, #k*64K
                    p.code.push(0xf900_02b3); // str x19, [x21]
                }
            });
        });
    }
}

fn pipes(c: &mut Criterion) {
    // A write takes what the pipe has room for (64 KiB): the 8 MiB one shows what is copied
    // beyond that
    for (name, len) in [
        ("pipe: write 64 KiB, read 64 KiB", 64u64 << 10),
        ("pipe: write 8 MiB (64 KiB taken), read 64 KiB", 8 << 20),
    ] {
        case(c, name, |p, n| {
            p.sys(59, &[D(0), I(0x800)]).expect(0); // pipe2(O_NONBLOCK)
            anon(p, 8 << 20, 8);
            repeat(p, n, |p| {
                p.sys(64, &[W(4), X(8), I(len)]);
                p.sys(63, &[W(0), X(8), I(64 << 10)]);
            });
        });
    }
    // What a change to a pipe costs with other processes blocked on another one
    for kids in [0u64, 16] {
        case(
            c,
            &format!("pipe: 1-byte write and read, {kids} processes blocked on another pipe"),
            |p, n| {
                p.sys(59, &[D(0), I(0)]).expect(0);
                p.sys(59, &[D(8), I(0)]).expect(0);
                for _ in 0..kids {
                    let at = p.fork();
                    p.sys(63, &[W(8), D(0x400), I(1)]);
                    p.parent(at);
                }
                repeat(p, n, |p| {
                    p.sys(64, &[W(4), D(0x400), I(1)]);
                    p.sys(63, &[W(0), D(0x400), I(1)]);
                });
            },
        );
    }
    case(c, "ppoll: 256 descriptors, none ready", |p, n| {
        p.sys(59, &[D(0), I(0)]).expect(0); // its read end is 3
        let fds: Vec<u8> = (0..256).flat_map(|_| [3u8, 0, 0, 0, 1, 0, 0, 0]).collect();
        p.put(0x800, &fds).put(0x700, &[0u8; 16]);
        repeat(p, n, |p| {
            p.sys(73, &[D(0x800), I(256), D(0x700), I(0), I(8)]);
        });
    });
}

criterion_group!(benches, syscalls, tlb, pipes);
criterion_main!(benches);
