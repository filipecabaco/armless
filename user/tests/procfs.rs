//! /proc and /sys as programs with no kernel read them: what top, htop, free, uptime and ps read
//! (the CPUs' and each process's CPU time, the load averages, the uptime, memory), what language
//! runtimes size themselves by (the CPUs online, cgroup limits, their own maps and auxiliary
//! vector), and the directories under /proc/<pid> (fd, task) with their links (cwd, root).
//!
//! Each program writes the files it reads to its output, a `--` line after each, and the test
//! reads them back.

mod prog;

use std::time::Duration;

use armless::vm::{Config, Vm};
use armless_user::{Process, Status};
use prog::*;

const AT_FDCWD: u64 = 0xffff_ff9c;
const O_DIRECTORY: u64 = 0o200000;
const O_RDWR: u64 = 2;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const ENOENT: i64 = -2;
const EISDIR: i64 = -21;
/// Where a file's contents are read to, and how much of it at most
const BUF: u32 = 0x2000;
const BUF_LEN: u64 = 0x1800;
/// The separator written after each file
const SEP: u32 = 0x1f0;
/// The CPUs a program is told of: 4 with one CPU, as many as there are with more
fn cpus_reported() -> usize {
    test_cpus().max(4)
}

type Machine = Vm<armless::platform::NativePlatform>;

impl Prog {
    /// The contents of the file at the path at `path` (openat from `dirfd`), then `--`
    fn show_at(&mut self, dirfd: A, path: u32) -> &mut Self {
        self.sys(56, &[dirfd, D(path), I(0), I(0)]).expect_ok().save(0x20);
        self.sys(63, &[X(0x20), D(BUF), I(BUF_LEN)]).expect_ok().save(0x28);
        self.sys(64, &[I(1), D(BUF), X(0x28)]);
        self.sys(57, &[X(0x20)]).expect(0);
        self.out(SEP, 4)
    }

    fn show(&mut self, path: u32) -> &mut Self {
        self.show_at(I(AT_FDCWD), path)
    }

    /// The names getdents64 gives for the directory at `path`, one a line, then `--`
    fn list(&mut self, path: u32) -> &mut Self {
        self.sys(56, &[I(AT_FDCWD), D(path), I(O_DIRECTORY), I(0)]).expect_ok().save(0x20);
        self.sys(61, &[X(0x20), D(BUF), I(BUF_LEN)]).expect_ok().save(0x28);
        self.sys(64, &[I(1), D(BUF), X(0x28)]);
        self.sys(57, &[X(0x20)]).expect(0);
        self.out(SEP, 4)
    }

    /// Spins until its process has had `ms` of CPU time (CLOCK_PROCESS_CPUTIME_ID): time to
    /// count, however fast the CPU runs the loop
    fn spin(&mut self, ms: u64) -> &mut Self {
        let top = self.code.len() as i64;
        self.sys(113, &[I(2), D(0x3f00)]); // clock_gettime
        self.arg(3, X(0x3f00)); // tv_sec
        self.arg(2, X(0x3f08)); // tv_nsec
        self.li(4, ms * 1_000_000);
        self.code.push(0xb500_0000 | 3 << 5 | 3); // cbnz x3, past the loop
        self.code.push(0xeb04_005f); // cmp x2, x4
        let back = (top - self.code.len() as i64) as u32 & 0x7_ffff;
        self.code.push(0x5400_0003 | back << 5); // b.lo top
        self
    }
}

fn prog() -> Prog {
    let mut p = Prog::default();
    p.put(SEP, b"\n--\n");
    p
}

/// What a program wrote, in the parts its `--` lines split it into
fn parts(out: &[u8]) -> Vec<Vec<u8>> {
    let mut text = Vec::new();
    for (i, &c) in out.iter().enumerate() {
        if !(c == b'\r' && out.get(i + 1) == Some(&b'\n')) {
            text.push(c);
        }
    }
    let mut parts = Vec::new();
    let mut rest = &text[..];
    while let Some(i) = rest.windows(4).position(|w| w == b"\n--\n") {
        parts.push(rest[..i].to_vec());
        rest = &rest[i + 4..];
    }
    parts.push(rest.to_vec());
    parts
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The names in the linux_dirent64 records in `b`
fn names(mut b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    while b.len() >= 19 {
        let reclen = u16::from_le_bytes([b[16], b[17]]) as usize;
        let name = b[19..reclen].split(|&c| c == 0).next().unwrap();
        out.push(text(name));
        b = &b[reclen..];
    }
    out
}

/// Runs `p` as `p`, with the environment `env`: what it wrote, in parts
fn run_with_env(p: Prog, env: &[&str]) -> Vec<Vec<u8>> {
    let mut vm = machine();
    let board = Some(vm.board.clone());
    let env: Vec<String> = env.iter().map(|e| e.to_string()).collect();
    let mut proc =
        Process::exec_bytes(&mut vm.cpus[0], board, &p.elf(), &["p".into()], &env).unwrap();
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    parts(&out)
}

/// A machine whose filesystem has the program at /bin/p and nothing else (no /proc)
fn machine_with_fs(elf: &[u8]) -> Machine {
    let config = Config {
        memory_size: 128 << 20,
        cpus: test_cpus(),
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let vm = armless::runner::new_vm(config).unwrap().0;
    let json = format!(
        r#"{{"version":3,"fsroot":[["bin",0,0,16877,0,0,[["p",{},0,33261,0,0,"p-blob"]]]]}}"#,
        elf.len()
    );
    vm.board.load_filesystem(json.as_bytes()).unwrap();
    vm.board.provide_raw_blob("p-blob", elf.to_vec()).unwrap();
    vm
}

/// The whitespace-separated fields of a /proc/<pid>/stat line (pid, comm, state, ...)
fn fields(stat: &str) -> Vec<String> {
    stat.split_whitespace().map(String::from).collect()
}

/// The 1-based field `n` of a stat line, as a number
fn field(stat: &str, n: usize) -> u64 {
    fields(stat)[n - 1].parse().unwrap()
}

#[test]
fn the_machines_files_say_its_cpus_time_load_and_memory() {
    let mut p = prog();
    p.put(0x200, b"/proc/stat\0").put(0x220, b"/proc/uptime\0");
    p.put(0x240, b"/proc/loadavg\0").put(0x260, b"/proc/meminfo\0");
    p.put(0x280, b"/proc/version\0").put(0x2a0, b"/sys/devices/system/cpu/online\0");
    p.put(0x2e0, b"/proc/sys/kernel/ostype\0").put(0x300, b"/proc/sys/kernel/pid_max\0");
    p.put(0x320, b"/sys/fs/cgroup/memory.max\0").put(0x350, b"/proc/filesystems\0");
    p.put(0x370, b"/sys/kernel/mm/transparent_hugepage/hpage_pmd_size\0");
    p.put(0x3c0, b"/proc/sys/kernel/random/boot_id\0");
    p.spin(50);
    for at in [0x200, 0x220, 0x240, 0x260, 0x280, 0x2a0, 0x2e0, 0x300, 0x320, 0x350, 0x370, 0x3c0] {
        p.show(at);
    }
    let parts = run_with_env(p, &[]);
    let n = cpus_reported();

    let stat = text(&parts[0]);
    let lines: Vec<&str> = stat.lines().collect();
    assert!(lines[0].starts_with("cpu  "), "{stat}");
    for c in 0..n {
        assert!(lines[1 + c].starts_with(&format!("cpu{c} ")), "{stat}");
        assert_eq!(lines[1 + c].split_whitespace().count(), 11, "{stat}");
    }
    assert!(!stat.contains(&format!("cpu{n} ")), "{stat}");
    // The spin is CPU time (user), counted on the CPUs; the rest of the time is idle
    let total: Vec<u64> = lines[0].split_whitespace().skip(1).map(|v| v.parse().unwrap()).collect();
    assert!(total[0] > 0, "user time: {stat}");
    assert!(total[3] > 0, "idle time: {stat}");
    for key in ["btime ", "processes 1000\n", "procs_running ", "ctxt "] {
        assert!(stat.contains(key), "{key:?} in {stat}");
    }

    // Uptime from the monotonic clock, which starts at a second; idle across the CPUs
    let uptime = text(&parts[1]);
    let up: Vec<f64> = uptime.split_whitespace().map(|v| v.parse().unwrap()).collect();
    assert_eq!(up.len(), 2, "{uptime}");
    assert!(up[0] >= 1.0 && up[0] < 60.0 && up[1] > 0.0, "{uptime}");

    let load = text(&parts[2]);
    let l: Vec<&str> = load.split_whitespace().collect();
    assert_eq!(l.len(), 5, "{load}");
    for v in &l[..3] {
        assert!(v.parse::<f64>().unwrap() >= 0.0, "{load}");
    }
    assert_eq!(l[3], "1/1", "running/threads: {load}");
    assert_eq!(l[4], "1000", "last pid: {load}");

    let mem = text(&parts[3]);
    assert!(mem.starts_with("MemTotal:         131072 kB\nMemFree:"), "{mem}");
    for key in ["MemAvailable:", "Buffers:", "Cached:", "SwapTotal:             0 kB", "SwapFree:", "Shmem:", "SReclaimable:", "AnonPages:"] {
        assert!(mem.contains(key), "{key:?} in {mem}");
    }

    assert!(text(&parts[4]).starts_with("Linux version 6.6.0-armless-user "), "{}", text(&parts[4]));
    assert_eq!(text(&parts[5]), format!("0-{}\n", n - 1));
    assert_eq!(text(&parts[6]), "Linux\n");
    assert_eq!(text(&parts[7]), "4194304\n");
    assert_eq!(text(&parts[8]), "max\n");
    assert!(text(&parts[9]).contains("nodev\tproc\n"));
    assert_eq!(text(&parts[10]), "2097152\n");
    let boot_id = text(&parts[11]);
    assert_eq!((boot_id.len(), &boot_id[14..15]), (37, "4"), "{boot_id}");
}

#[test]
fn a_process_s_own_files_say_its_cpu_time_start_environment_auxv_and_maps() {
    let mut p = prog();
    p.put(0x200, b"/proc/self/stat\0").put(0x220, b"/proc/self/environ\0");
    p.put(0x240, b"/proc/self/auxv\0").put(0x260, b"/proc/self/maps\0");
    p.put(0x280, b"/proc/self/limits\0").put(0x2a0, b"/proc/self/io\0");
    p.put(0x2c0, b"/proc/self/cgroup\0").put(0x2e0, b"/proc/self/mountinfo\0");
    p.put(0x300, b"/proc/self/status\0");
    p.spin(50);
    for at in [0x200, 0x220, 0x240, 0x260, 0x280, 0x2a0, 0x2c0, 0x2e0, 0x300] {
        p.show(at);
    }
    // times(): the process's ticks and its children's (none), and getrusage(RUSAGE_SELF)'s
    // ru_utime, then sysinfo's uptime and mem_unit
    p.sys(153, &[D(0x400)]).expect_ok();
    p.out(0x400, 32).out(SEP, 4);
    p.sys(165, &[I(0), D(0x500)]).expect(0);
    p.out(0x500, 16).out(SEP, 4);
    p.sys(179, &[D(0x600)]).expect(0);
    p.out(0x600, 112);
    let parts = run_with_env(p, &["A=1", "HOME=/h"]);

    let stat = text(&parts[0]);
    assert_eq!(fields(&stat).len(), 52, "{stat}");
    assert!(stat.starts_with("1000 (p) R 1 1000 1000 "), "{stat}");
    assert!(field(&stat, 14) > 0, "utime: {stat}");
    assert_eq!(field(&stat, 15), 0, "stime: {stat}");
    // It started with the machine: the monotonic clock's first second
    assert_eq!(field(&stat, 22), 100, "starttime: {stat}");
    assert_eq!(field(&stat, 25), u64::MAX, "rsslim: {stat}");
    assert!(field(&stat, 39) < test_cpus() as u64, "processor: {stat}");

    assert_eq!(parts[1], b"A=1\0HOME=/h\0");

    let auxv = &parts[2];
    assert_eq!(auxv.len() % 16, 0);
    let pairs: Vec<(u64, u64)> = auxv
        .chunks(16)
        .map(|c| (u64::from_le_bytes(c[..8].try_into().unwrap()), u64::from_le_bytes(c[8..].try_into().unwrap())))
        .collect();
    assert_eq!(pairs.last(), Some(&(0, 0)), "AT_NULL last");
    assert!(pairs.contains(&(6, 4096)), "AT_PAGESZ: {pairs:?}");
    assert!(pairs.contains(&(9, BASE + CODE)), "AT_ENTRY: {pairs:?}");

    let maps = text(&parts[3]);
    let lines: Vec<&str> = maps.lines().collect();
    // The program's one segment, with what it may do (its own: read, write and execute)
    assert!(lines.iter().any(|l| l.starts_with("00400000-") && l.contains("xp 00000000 ")), "{maps}");
    let stack: Vec<&&str> = lines.iter().filter(|l| l.ends_with("[stack]")).collect();
    assert_eq!(stack.len(), 1, "{maps}");
    // Each line: start-end perms offset dev inode [path], the areas in order
    let mut last = 0;
    for l in &lines {
        let f: Vec<&str> = l.split_whitespace().collect();
        assert!(f.len() >= 5, "{l}");
        let (s, e) = f[0].split_once('-').unwrap();
        let (s, e) = (u64::from_str_radix(s, 16).unwrap(), u64::from_str_radix(e, 16).unwrap());
        assert!(s < e && s >= last, "{maps}");
        last = e;
        assert_eq!(f[1].len(), 4, "{l}");
        assert_eq!(f[3], "00:00", "{l}");
    }

    let limits = text(&parts[4]);
    assert_eq!(limits.lines().count(), 17, "{limits}");
    assert!(limits.contains("Max open files            4096"), "{limits}");
    assert!(text(&parts[5]).starts_with("rchar: 0\nwchar: 0\n"));
    assert_eq!(text(&parts[6]), "0::/\n");
    assert!(text(&parts[7]).contains(" / / rw - 9p host9p rw\n"));
    let status = text(&parts[8]);
    for line in ["Cpus_allowed_list:\t0-", "voluntary_ctxt_switches:\t", "NSpid:\t1000\n", "VmSwap:\t0 kB\n"] {
        assert!(status.contains(line), "{line:?} in {status}");
    }

    let u = |b: &[u8], i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
    let (tms, ru, si) = (&parts[9], &parts[10], &parts[11]);
    assert!(u(tms, 0) > 0, "tms_utime");
    assert_eq!((u(tms, 1), u(tms, 2), u(tms, 3)), (0, 0, 0), "stime, cutime, cstime");
    // ru_utime: the same CPU time as times() (to a tick), a little more for the calls between
    let ru_ns = u(ru, 0) * 1_000_000_000 + u(ru, 1) * 1000;
    assert!(ru_ns / 10_000_000 >= u(tms, 0), "{ru_ns} ns, {} ticks", u(tms, 0));
    assert!(field(&stat, 14) <= u(tms, 0));
    assert!(u(si, 0) >= 1, "sysinfo uptime");
    assert_eq!(u32::from_le_bytes(si[104..108].try_into().unwrap()), 1, "mem_unit");
}

#[test]
fn a_child_s_cpu_time_is_its_parent_s_cutime_once_it_ended() {
    let mut p = prog();
    p.put(0x200, b"/proc/self/stat\0");
    let child = p.fork();
    p.spin(50);
    p.parent(child);
    p.sys(260, &[I(u64::MAX), I(0), I(0), I(0)]).expect(1001); // wait4(-1)
    p.show(0x200);
    p.sys(153, &[D(0x400)]).expect_ok();
    p.out(0x400, 32);
    let parts = run_with_env(p, &[]);
    let stat = text(&parts[0]);
    assert!(field(&stat, 16) > 0, "cutime: {stat}");
    let cutime = u64::from_le_bytes(parts[1][16..24].try_into().unwrap());
    assert_eq!(cutime, field(&stat, 16));
}

#[test]
fn proc_pid_and_the_directories_under_it_list_and_open() {
    let mut p = prog();
    p.put(0x200, b"/proc/self\0").put(0x210, b"/proc/self/fd\0");
    p.put(0x230, b"/proc/1000/task/\0").put(0x250, b"/proc/1000/task/1000/stat\0");
    p.put(0x280, b"status\0").put(0x290, b"/proc/self/cwd\0");
    p.put(0x2b0, b"/proc/1000/root\0").put(0x2d0, b"/proc/1000/fdinfo/1\0");
    p.put(0x2f0, b"/proc/1000/task/1001/stat\0").put(0x320, b"/sys/devices/system/cpu\0");
    p.put(0x340, b"/proc/1000/exe\0");
    p.list(0x200).list(0x210).list(0x230).show(0x250);
    // "status" from a descriptor of /proc/self: its own status
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_DIRECTORY), I(0)]).expect(3);
    p.show_at(I(3), 0x280);
    // A directory made here: fstat says so, and it opens for reading only
    p.sys(80, &[I(3), D(0x600)]).expect(0);
    p.out(0x600 + 16, 4).out(SEP, 4);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_RDWR), I(0)]).expect(EISDIR);
    // Links: readlink, and newfstatat that doesn't follow them
    for at in [0x290, 0x2b0, 0x340] {
        p.sys(78, &[I(AT_FDCWD), D(at), D(0x700), I(64)]).expect_ok().save(0x28);
        p.sys(64, &[I(1), D(0x700), X(0x28)]);
        p.out(SEP, 4);
    }
    p.sys(79, &[I(AT_FDCWD), D(0x290), D(0x600), I(AT_SYMLINK_NOFOLLOW)]).expect(0);
    p.out(0x600 + 16, 4).out(SEP, 4);
    p.show(0x2d0);
    p.sys(56, &[I(AT_FDCWD), D(0x2f0), I(0), I(0)]).expect(ENOENT);
    p.list(0x320);
    let parts = run_with_env(p, &[]);

    let own = names(&parts[0]);
    for n in [".", "..", "stat", "status", "maps", "environ", "fd", "task", "exe", "cwd", "root"] {
        assert!(own.iter().any(|x| x == n), "{n} in {own:?}");
    }
    // 0, 1 and 2, and the descriptor that lists them
    assert_eq!(names(&parts[1]), [".", "..", "0", "1", "2", "3"]);
    assert_eq!(names(&parts[2]), [".", "..", "1000"]);
    assert!(text(&parts[3]).starts_with("1000 (p) R "), "{}", text(&parts[3]));
    assert!(text(&parts[4]).starts_with("Name:\tp\n"), "{}", text(&parts[4]));
    let mode = u32::from_le_bytes(parts[5][..4].try_into().unwrap());
    assert_eq!(mode & 0o170000, 0o040000, "S_IFDIR");
    assert_eq!(parts[6], b"/");
    assert_eq!(parts[7], b"/");
    // (Started from its bytes, with no file: its name)
    assert_eq!(parts[8], b"p");
    let mode = u32::from_le_bytes(parts[9][..4].try_into().unwrap());
    assert_eq!(mode & 0o170000, 0o120000, "S_IFLNK");
    assert!(text(&parts[10]).starts_with("pos:\t0\nflags:\t"), "{}", text(&parts[10]));
    let cpu = names(&parts[11]);
    for n in ["online", "possible", "present", "kernel_max"] {
        assert!(cpu.iter().any(|x| x == n), "{n} in {cpu:?}");
    }
}

#[test]
fn top_s_chdir_to_proc_works_with_no_proc_in_the_image() {
    // busybox's top: chdir("/proc"), then "stat", "meminfo" and "<pid>/stat" from there
    let mut p = prog();
    p.put(0x200, b"/proc\0").put(0x210, b"stat\0").put(0x220, b"meminfo\0");
    p.put(0x230, b"1000/stat\0").put(0x240, b"self/fd\0");
    p.sys(49, &[D(0x200)]).expect(0); // chdir
    p.show(0x210).show(0x220).show(0x230);
    p.sys(56, &[I(AT_FDCWD), D(0x240), I(O_DIRECTORY), I(0)]).expect(3);
    let elf = p.elf();
    let mut vm = machine_with_fs(&elf);
    let board = vm.board.clone();
    let mut proc = Process::exec(&mut vm.cpus[0], board, &["/bin/p".into()], &[], "/").unwrap();
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    let parts = parts(&out);
    assert!(text(&parts[0]).starts_with("cpu  "), "{}", text(&parts[0]));
    assert!(text(&parts[1]).starts_with("MemTotal:"));
    assert!(text(&parts[2]).starts_with("1000 (p) R "));
}

/// `p` started with the environment `A=1`, run until it waits for input, saved: the machine's
/// state and the process's
fn saved_waiting(p: Prog) -> (Vec<u8>, Vec<u8>) {
    let mut vm = machine_with_cpus(1);
    let board = Some(vm.board.clone());
    let mut proc =
        Process::exec_bytes(&mut vm.cpus[0], board, &p.elf(), &["p".into()], &["A=1".into()]).unwrap();
    let status = loop {
        match proc.run_on(&mut vm.cpus, Duration::from_millis(20)) {
            Status::Running => {},
            s => break s,
        }
    };
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    (vm.save_state(), proc.save(&vm.cpus[0]))
}

/// Restores `process` (after `asleep`), gives it a line and runs it to its end: what it wrote
fn restored(state: &[u8], process: &[u8], asleep: Duration) -> Vec<Vec<u8>> {
    let mut vm = machine_with_cpus(1);
    vm.load_state(state).unwrap();
    let mut proc = Process::restore(&mut vm.cpus[0], None, process, asleep).unwrap();
    proc.input(b"\n");
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    // (Without the line the terminal echoed)
    parts(out.strip_prefix(b"\r\n").unwrap_or(&out))
}

#[test]
fn a_restored_process_keeps_its_start_environment_and_auxv() {
    let mut p = prog();
    p.put(0x200, b"/proc/self/environ\0").put(0x220, b"/proc/self/stat\0");
    p.put(0x240, b"/proc/self/auxv\0").put(0x260, b"/proc/uptime\0");
    p.sys(63, &[I(0), D(0x380), I(1)]).expect(1);
    p.show(0x200).show(0x220).show(0x240).show(0x260);
    let (state, process) = saved_waiting(p);
    let parts = restored(&state, &process, Duration::from_secs(3600));
    assert_eq!(parts[0], b"A=1\0");
    // It started when it did, an hour before the uptime it reads now
    let stat = text(&parts[1]);
    assert_eq!(field(&stat, 22), 100, "{stat}");
    let up: f64 = text(&parts[3]).split_whitespace().next().unwrap().parse().unwrap();
    assert!(up >= 3601.0, "{up}");
    assert!(parts[2].len() >= 32 && parts[2].ends_with(&[0; 16]));
}

#[test]
fn a_state_saved_before_armlusrc_loads_with_no_environment_nor_auxv() {
    let mut p = prog();
    p.put(0x200, b"/proc/self/environ\0").put(0x220, b"/proc/self/auxv\0");
    p.put(0x240, b"/proc/self/stat\0");
    p.sys(63, &[I(0), D(0x380), I(1)]).expect(1);
    p.show(0x200).show(0x220).show(0x240);
    let (state, mut process) = saved_waiting(p);
    to_armlusrb(&mut process);
    let parts = restored(&state, &process, Duration::ZERO);
    assert_eq!((parts[0].len(), parts[1].len()), (0, 0));
    // The machine's start
    assert_eq!(field(&text(&parts[2]), 22), 100);
}

#[test]
fn a_state_with_a_section_of_the_wrong_length_is_refused() {
    let mut p = prog();
    p.sys(63, &[I(0), D(0x380), I(1)]).expect(1);
    let (state, mut process) = saved_waiting(p);
    let n = process.len();
    let len = u64::from_le_bytes(process[n - 8..].try_into().unwrap());
    process[n - 8..].copy_from_slice(&(len + 8).to_le_bytes());
    let mut vm = machine_with_cpus(1);
    vm.load_state(&state).unwrap();
    let err = Process::restore(&mut vm.cpus[0], None, &process, Duration::ZERO).err();
    assert_eq!(err.as_deref(), Some("a section of the wrong length"));
}

#[test]
fn each_thread_has_a_directory_in_task_and_counts_in_loadavg() {
    // A second thread (clone with CLONE_VM | CLONE_SIGHAND | CLONE_THREAD) that waits on a pipe
    // nobody writes; the first lists task/, reads the thread's stat and the load
    let mut p = prog();
    p.put(0x200, b"/proc/self/task\0").put(0x220, b"/proc/self/task/1001/stat\0");
    p.put(0x250, b"/proc/self/task/1001/status\0").put(0x280, b"/proc/loadavg\0");
    p.put(0x2a0, b"/proc/self/status\0");
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3, 4
    p.sys(220, &[I(0x10900), I(0), I(0), I(0), I(0)]);
    let at = p.code.len();
    p.code.push(0); // cbnz x0 over the thread's part
    p.sys(63, &[I(3), D(0x300), I(1)]);
    p.sys(93, &[I(0)]);
    let skip = (p.code.len() - at) as u32;
    p.code[at] = 0xb500_0000 | skip << 5;
    p.sys(124, &[]); // sched_yield: the thread runs to its read
    p.list(0x200).show(0x220).show(0x250).show(0x280).show(0x2a0);
    let parts = run_with_env(p, &[]);
    assert_eq!(names(&parts[0]), [".", "..", "1000", "1001"]);
    let stat = text(&parts[1]);
    assert_eq!(fields(&stat).len(), 52, "{stat}");
    // Its own id, its process's comm, parent and group
    assert!(stat.starts_with("1001 (p) S 1 1000 ") || stat.starts_with("1001 (p) R 1 1000 "), "{stat}");
    assert_eq!(field(&stat, 20), 2, "num_threads: {stat}");
    let status = text(&parts[2]);
    assert!(status.contains("Pid:\t1001\n") && status.contains("Tgid:\t1000\n"), "{status}");
    let load = text(&parts[3]);
    assert!(load.split_whitespace().nth(3).unwrap().ends_with("/2"), "{load}");
    assert!(text(&parts[4]).contains("Threads:\t2\n"));
}
