//! What a process knows of its own memory: /proc/<pid>/status (VmSize, VmRSS, VmHWM, RssAnon, ...),
//! statm, stat's vsize and rss, getrusage's ru_maxrss, and /proc/meminfo's free memory. Counted
//! from the process's page tables: the pages that have a frame mapped.
//!
//! Each program dumps /proc/self/status to its stdout at the points that matter (`dump`), and the
//! test reads the numbers back.

mod prog;

use prog::*;

const PAGE_KB: u64 = 4;
const PATH: u32 = 0x40;
/// Where the status text is read to, and getrusage's struct
const BUF: u32 = 0x1000;
const RUSAGE: u32 = 0x1800;
const ANON: u64 = 0x22;
const NORESERVE: u64 = 0x4000;

/// str xzr, [x10]
const STORE: u32 = 0xf900_015f;
/// add x10, x10, #4096
const NEXT_PAGE: u32 = 0x9140_054a;

fn program() -> Prog {
    let mut p = Prog::default();
    p.put(PATH, b"/proc/self/status\0");
    p.put(0x80, b"/proc/self/statm\0");
    p.put(0xa0, b"/proc/meminfo\0");
    p
}

impl Prog {
    /// write(1, the contents of the file at `path`), at most 2 kB
    fn cat(&mut self, path: u32) -> &mut Self {
        self.sys(56, &[I(-100i64 as u64), D(path), I(0)]).save(0x20);
        self.sys(63, &[X(0x20), D(BUF), I(0x800)]).save(0x28);
        self.sys(64, &[I(1), D(BUF), X(0x28)]);
        self.sys(57, &[X(0x20)])
    }

    /// /proc/self/status to the output
    fn dump(&mut self) -> &mut Self {
        self.cat(PATH)
    }

    /// An anonymous mapping of `len` bytes (its address kept at 0x30 + 8 * slot)
    fn map(&mut self, slot: u32, len: u64, prot: u64, extra: u64) -> &mut Self {
        self.sys(
            222,
            &[I(0), I(len), I(prot), I(ANON | extra), I(u64::MAX), I(0)],
        )
        .expect_ok()
        .save(0x30 + 8 * slot)
    }

    /// Writes to each of the first `pages` pages of the mapping in `slot`
    fn touch(&mut self, slot: u32, pages: u32) -> &mut Self {
        self.arg(10, X(0x30 + 8 * slot));
        for _ in 0..pages {
            self.code.push(STORE);
            self.code.push(NEXT_PAGE);
        }
        self
    }

    /// getrusage(who), ru_maxrss (8 bytes) to the output
    fn maxrss(&mut self, who: u64) -> &mut Self {
        self.sys(165, &[I(who), D(RUSAGE)]).expect(0);
        self.out(RUSAGE + 32, 8)
    }
}

/// The status dumps in what a program wrote: a dump starts at its `Name:`
fn dumps(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .split("Name:")
        .skip(1)
        .map(|d| format!("Name:{d}"))
        .collect()
}

/// A line's number (kB for the memory ones)
fn field(dump: &str, name: &str) -> u64 {
    dump.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("no {name} in\n{dump}"))
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .unwrap()
}

fn run_prog(p: Prog) -> Vec<u8> {
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    out
}

#[test]
fn touching_pages_raises_vmrss_and_unmapping_lowers_it() {
    let mut p = program();
    p.dump(); // (the first dump touches its own buffer)
    p.map(0, 64 << 12, 3, 0);
    p.dump();
    p.touch(0, 64);
    p.dump();
    p.sys(215, &[X(0x30), I(64 << 12)]).expect(0);
    p.dump();
    let d = dumps(&run_prog(p));
    assert_eq!(d.len(), 4);
    let rss: Vec<u64> = d.iter().map(|d| field(d, "VmRSS")).collect();
    assert!(rss[0] > 0, "the program has pages: {rss:?}");
    // Mapped, not touched: no more resident, but a larger size
    assert_eq!(field(&d[1], "VmSize") - field(&d[0], "VmSize"), 64 * PAGE_KB);
    // Touched: N pages, N x 4 kB, all of them anonymous
    assert_eq!(rss[2] - rss[1], 64 * PAGE_KB);
    assert_eq!(field(&d[2], "RssAnon") - field(&d[1], "RssAnon"), 64 * PAGE_KB);
    assert_eq!(field(&d[2], "RssFile"), field(&d[1], "RssFile"));
    // The parts add up to the whole
    for d in &d {
        assert_eq!(
            field(d, "RssAnon") + field(d, "RssFile") + field(d, "RssShmem"),
            field(d, "VmRSS")
        );
    }
    // Unmapped: back to what it was, and the size too; the peak stays
    assert_eq!(rss[3], rss[1]);
    assert_eq!(field(&d[3], "VmSize"), field(&d[0], "VmSize"));
    assert_eq!(field(&d[3], "VmHWM"), rss[2]);
    // The stack is what it has used, not the 8 MB it may grow into
    assert!(field(&d[0], "VmStk") < 256, "{}", field(&d[0], "VmStk"));
    // The peak size: what it was with the mapping, which is gone
    assert_eq!(field(&d[3], "VmPeak"), field(&d[1], "VmSize"));
    assert!(field(&d[3], "VmPeak") > field(&d[3], "VmSize"));
    assert!(d.iter().all(|d| field(d, "VmPeak") >= field(d, "VmSize")));
}

#[test]
fn a_vfork_child_that_ends_leaves_its_parents_peak_out_of_the_childrens() {
    // The child of a vfork has its parent's address space until it execs: what the space held
    // is not the child's
    let mut p = program();
    p.map(0, 64 << 12, 3, 0).touch(0, 64);
    // clone(CLONE_VM | CLONE_VFORK | SIGCHLD): the child ends at once
    p.sys(220, &[I(0x4111), I(0), I(0), I(0), I(0)]);
    p.code.push(0);
    let child = p.code.len() - 1;
    p.parent(child);
    p.sys(260, &[I(u64::MAX), I(0), I(0), I(0)]);
    p.maxrss(u64::MAX);
    let out = run_prog(p);
    assert_eq!(u64::from_le_bytes(out[..8].try_into().unwrap()), 0);
}

#[test]
fn madvise_dontneed_lowers_vmrss_but_not_vmsize() {
    let mut p = program();
    p.dump(); // (the first dump touches its own buffer)
    p.map(0, 32 << 12, 3, 0).touch(0, 32);
    p.dump();
    p.sys(233, &[X(0x30), I(16 << 12), I(4)]).expect(0);
    p.dump();
    let d = dumps(&run_prog(p));
    assert_eq!(field(&d[1], "VmRSS") - field(&d[2], "VmRSS"), 16 * PAGE_KB);
    assert_eq!(field(&d[1], "VmSize"), field(&d[2], "VmSize"));
    assert_eq!(field(&d[2], "VmHWM"), field(&d[1], "VmRSS"));
}

#[test]
fn a_big_reservation_costs_size_not_memory() {
    let gib = 1u64 << 30;
    let mut p = program();
    p.dump(); // (the first dump touches its own buffer)
    p.dump();
    p.map(0, gib, 0, NORESERVE);
    p.dump();
    // Touched pages of a reservation, once it can be written, are resident
    p.sys(226, &[X(0x30), I(8 << 12), I(3)]).expect(0);
    p.touch(0, 8);
    p.dump();
    let d = dumps(&run_prog(p));
    let d = &d[1..];
    assert_eq!(field(&d[1], "VmRSS"), field(&d[0], "VmRSS"));
    assert_eq!(field(&d[1], "VmSize") - field(&d[0], "VmSize"), gib / 1024);
    assert_eq!(field(&d[2], "VmRSS") - field(&d[1], "VmRSS"), 8 * PAGE_KB);
    assert_eq!(field(&d[2], "VmSize"), field(&d[1], "VmSize"));
}

#[test]
fn after_a_fork_both_processes_have_the_shared_pages() {
    let mut p = program();
    p.map(0, 48 << 12, 3, 0).touch(0, 48);
    p.dump(); // the parent's, before
    let child = p.fork();
    p.dump();
    p.parent(child);
    p.sys(260, &[I(u64::MAX), I(0), I(0), I(0)]);
    p.dump();
    let d = dumps(&run_prog(p));
    assert_eq!(d.len(), 3);
    let before = field(&d[0], "VmRSS");
    assert_ne!(field(&d[1], "Pid"), field(&d[2], "Pid"), "one dump each");
    // The copy-on-write pages count in each, as Linux's RSS does
    for d in &d[1..] {
        assert!(
            field(d, "VmRSS") >= before,
            "pid {} has {} kB, the parent had {before}",
            field(d, "Pid"),
            field(d, "VmRSS")
        );
        assert!(field(d, "RssAnon") >= 48 * PAGE_KB);
    }
}

#[test]
fn ru_maxrss_keeps_the_peak() {
    let mut p = program();
    p.sys(165, &[I(0), D(RUSAGE)]); // (the call touches its struct's page)
    p.map(0, 64 << 12, 3, 0);
    p.maxrss(0);
    p.touch(0, 64);
    p.maxrss(0);
    p.sys(215, &[X(0x30), I(64 << 12)]).expect(0);
    p.maxrss(0);
    p.maxrss(u64::MAX);
    // A child that had a mapping resident when it ended: the parent's RUSAGE_CHILDREN has it
    let child = p.fork();
    p.map(1, 128 << 12, 3, 0).touch(1, 128);
    p.parent(child);
    p.sys(260, &[I(u64::MAX), I(0), I(0), I(0)]);
    p.maxrss(u64::MAX);
    let out = run_prog(p);
    let v: Vec<u64> = out
        .chunks(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(v.len(), 5);
    assert_eq!(v[1] - v[0], 64 * PAGE_KB);
    assert_eq!(v[2], v[1], "ru_maxrss is the peak, and unmapping leaves it");
    assert_eq!(v[3], 0, "no child has ended");
    assert!(v[4] >= 128 * PAGE_KB, "the child's peak: {}", v[4]);
}

#[test]
fn statm_agrees_with_status() {
    let mut p = program();
    p.dump(); // (the first read touches its buffer)
    p.map(0, 16 << 12, 3, 0).touch(0, 16);
    p.cat(0x80);
    p.dump();
    let out = run_prog(p);
    // The warm-up's dump with the statm line after it, then the status
    let d = dumps(&out);
    let status = &d[1];
    let statm = d[0].lines().last().unwrap();
    let f: Vec<u64> = statm
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(f.len(), 7, "{statm:?}");
    assert_eq!(f[0] * PAGE_KB, field(status, "VmSize"));
    assert_eq!(f[1] * PAGE_KB, field(status, "VmRSS"));
    assert!(f[1] >= 16);
}

#[test]
fn meminfo_free_memory_falls_with_what_is_in_use() {
    let mut p = program();
    p.cat(0xa0);
    p.map(0, 256 << 12, 3, 0).touch(0, 256);
    p.cat(0xa0);
    let out = run_prog(p);
    let text = String::from_utf8_lossy(&out).into_owned();
    let num = |l: &str| -> u64 { l.trim().trim_end_matches("kB").trim().parse().unwrap() };
    let free: Vec<u64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("MemFree:"))
        .map(num)
        .collect();
    assert_eq!(free.len(), 2, "{text}");
    let total = text
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .map(num)
        .unwrap();
    assert!(free[0] < total && free[0] > total / 2, "{free:?} of {total}");
    // The 256 pages, and a page table or two
    let used = free[0] - free[1];
    assert!((256 * PAGE_KB..=256 * PAGE_KB + 16).contains(&used), "{used} kB");
}
