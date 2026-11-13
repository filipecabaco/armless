//! What user mode answers as Linux would for programs that run as services: user and group ids,
//! /proc/self/exe, signalfd, the interval timer, System V shared memory, and futexes in memory
//! that processes share. Postgres needs every one of them to start; Java the second.

mod prog;

use armless::vm::{Config, Vm};
use armless_user::Process;
use prog::*;

const SIGUSR1: u64 = 10;
const SIGALRM: u64 = 14;
/// rt_sigprocmask's SIG_BLOCK
const SIG_BLOCK: u64 = 0;
const SFD_NONBLOCK: u64 = 0o4000;
const EPERM: i64 = -1;
const EAGAIN: i64 = -11;

/// A machine whose filesystem has /usr/lib/real/prog (`elf`), the link /usr/bin/prog to it, and
/// an empty /tmp.
fn machine_with_prog(elf: &[u8]) -> Vm<armless::platform::NativePlatform> {
    let config = Config {
        memory_size: 128 << 20,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let vm = armless::runner::new_vm(config).unwrap().0;
    let json = format!(
        r#"{{"version":3,"fsroot":[
            ["usr",0,0,16877,0,0,[
                ["lib",0,0,16877,0,0,[["real",0,0,16877,0,0,[["prog",{},0,33261,0,0,"prog-blob"]]]]],
                ["bin",0,0,16877,0,0,[["prog",16,0,41471,0,0,"../lib/real/prog"]]]]],
            ["tmp",0,0,17407,0,0,[]]]}}"#,
        elf.len()
    );
    vm.board.load_filesystem(json.as_bytes()).unwrap();
    vm.board.provide_raw_blob("prog-blob", elf.to_vec()).unwrap();
    vm
}

/// Runs `p` as /usr/bin/prog, which links to it
fn run_linked(p: Prog) -> (Vec<u8>, armless_user::Status) {
    let mut vm = machine_with_prog(&p.elf());
    let board = vm.board.clone();
    let mut proc = Process::exec(&mut vm.cpus[0], board, &["/usr/bin/prog".into()], &[], "/").unwrap();
    run(&mut proc, &mut vm)
}

#[test]
fn proc_self_exe_is_the_file_that_runs_with_its_links_followed() {
    // musl's loader finds $ORIGIN from it: /usr/bin/java is a link into the JDK, whose libjli.so
    // is beside the real java
    let mut p = Prog::default();
    p.put(0x200, b"/proc/self/exe\0");
    p.sys(78, &[I(0xffff_ff9c), D(0x200), D(0x300), I(64)]) // readlinkat(AT_FDCWD, ...)
        .expect(18);
    p.out(0x300, 18);
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"/usr/lib/real/prog");
}

#[test]
fn a_process_becomes_another_user_and_its_files_are_that_users() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/mine\0");
    p.sys(174, &[]).expect(0); // getuid: root to start with
    p.sys(159, &[I(0), I(0)]).expect(0); // setgroups: root may
    p.sys(149, &[I(70), I(70), I(70)]).expect(0); // setresgid
    p.sys(147, &[I(70), I(70), I(70)]).expect(0); // setresuid
    p.sys(174, &[]).expect(70);
    p.sys(175, &[]).expect(70);
    p.sys(176, &[]).expect(70);
    p.sys(177, &[]).expect(70);
    // Not root any more: no way back, and no groups to set
    p.sys(146, &[I(0)]).expect(EPERM);
    p.sys(159, &[I(0), I(0)]).expect(EPERM);
    // What it makes is its own: openat(AT_FDCWD, O_WRONLY | O_CREAT), then fstat's st_uid, st_gid
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o101), I(0o644)]).expect(3);
    p.sys(80, &[I(3), D(0x400)]).expect(0);
    p.arg(0, W(0x400 + 24));
    p.expect(70);
    p.arg(0, W(0x400 + 28));
    p.expect(70);
    // getresuid
    p.sys(148, &[D(0x500), D(0x504), D(0x508)]).expect(0);
    p.arg(0, W(0x508));
    p.expect(70);
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn a_signalfd_reads_the_blocked_signals_that_are_pending() {
    let mut p = Prog::default();
    p.put(0x100, &(1u64 << (SIGUSR1 - 1)).to_le_bytes());
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(74, &[I(0xffff_ffff), D(0x100), I(8), I(SFD_NONBLOCK)])
        .expect(3);
    // Nothing yet
    p.sys(63, &[I(3), D(0x400), I(128)]).expect(EAGAIN);
    // kill(getpid(), SIGUSR1): pending, blocked, so the descriptor has it
    p.sys(172, &[]).save(0x180);
    p.sys(129, &[W(0x180), I(SIGUSR1)]).expect(0);
    p.sys(63, &[I(3), D(0x400), I(128)]).expect(128);
    p.arg(0, W(0x400)); // ssi_signo
    p.expect(SIGUSR1 as i64);
    // Taken: it is not delivered as well
    p.sys(63, &[I(3), D(0x400), I(128)]).expect(EAGAIN);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn the_interval_timer_raises_sigalrm_when_it_is_due() {
    let mut p = Prog::default();
    p.put(0x100, &(1u64 << (SIGALRM - 1)).to_le_bytes());
    // struct itimerval: no interval, 20 ms
    p.put(0x200, &[0u8; 24]).put(0x218, &20_000u64.to_le_bytes());
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(74, &[I(0xffff_ffff), D(0x100), I(8), I(0)]).expect(3);
    p.sys(103, &[I(0), D(0x200), I(0)]).expect(0); // setitimer(ITIMER_REAL)
    // getitimer: armed, with time left
    p.sys(102, &[I(0), D(0x300)]).expect(0);
    p.arg(0, W(0x318));
    p.code.push(0xb500_0000 | 4 << 5); // cbnz x0, +16: past the exit below
    p.sys(94, &[I(99)]);
    // The read waits for the alarm
    p.sys(63, &[I(3), D(0x400), I(128)]).expect(128);
    p.arg(0, W(0x400));
    p.expect(SIGALRM as i64);
    // Once only: disarmed now
    p.sys(102, &[I(0), D(0x300)]).expect(0);
    p.arg(0, W(0x318));
    p.expect(0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn shared_memory_segments_are_the_same_memory_in_every_process_that_attaches_them() {
    let mut p = Prog::default();
    // A pipe with what the child puts in the segment: a system call writes it there
    p.put(0x200, b"shared");
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3, 4
    p.sys(64, &[I(4), D(0x200), I(6)]).expect(6);
    // shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600), shmat(id, NULL, 0)
    p.sys(194, &[I(0), I(4096), I(0o1600)]).save(0x110);
    p.sys(196, &[W(0x110), I(0), I(0)]).save(0x118);
    let child = p.fork();
    // The child attaches it again, at an address of its own, and fills it from the pipe
    p.sys(196, &[W(0x110), I(0), I(0)]).save(0x120);
    p.sys(63, &[I(3), X(0x120), I(6)]).expect(6);
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), I(0), I(0), I(0)]); // wait4(-1)
    // The parent's attach has it
    p.sys(64, &[I(1), X(0x118), I(6)]).expect(6);
    // Removed, then detached: the segment goes with its last attach
    p.sys(195, &[W(0x110), I(0), I(0)]).expect(0); // IPC_RMID
    p.sys(197, &[X(0x118)]).expect(0); // shmdt
    p.sys(197, &[X(0x118)]).expect(-22); // EINVAL: not attached there any more
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    assert_eq!(out, b"shared");
}

#[test]
fn a_futex_in_shared_memory_wakes_a_waiter_in_another_process() {
    // Postgres's semaphores: one process waits on a word of its shared memory, another wakes it
    let mut p = Prog::default();
    p.put(0x200, &1u32.to_le_bytes());
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3, 4
    p.sys(64, &[I(4), D(0x200), I(4)]).expect(4);
    // mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0)
    p.sys(222, &[I(0), I(4096), I(3), I(0x21), I(0xffff_ffff), I(0)])
        .save(0x118);
    let child = p.fork();
    // The child sets the word (from the pipe) and wakes whoever waits on it: the parent
    p.sys(63, &[I(3), X(0x118), I(4)]).expect(4);
    p.sys(98, &[X(0x118), I(1), I(1)]).expect(1); // FUTEX_WAKE, not private
    p.parent(child);
    // The parent runs first after the fork and waits while the word is 0
    p.sys(98, &[X(0x118), I(0), I(0), I(0)]).expect(0); // FUTEX_WAIT
    p.sys(260, &[I(0xffff_ffff), D(0x300), I(0), I(0)]);
    p.arg(0, W(0x300));
    p.expect(0); // the child exited 0: its wake found the parent
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn a_file_mapped_shared_is_the_same_memory_in_every_process_that_maps_it() {
    // Postgres's dynamic shared memory: files in pg_dynshmem its processes map MAP_SHARED
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/dsm\0").put(0x220, b"in the file");
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3, 4
    p.sys(64, &[I(4), D(0x220), I(11)]).expect(11);
    // openat(AT_FDCWD, "/tmp/dsm", O_RDWR | O_CREAT), ftruncate to a page
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o600)]).expect(5);
    p.sys(46, &[I(5), I(4096)]).expect(0);
    // mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, 5, 0): untouched before the fork, so
    // each side has its pages from the file when it first touches them
    p.sys(222, &[I(0), I(4096), I(3), I(1), I(5), I(0)]).save(0x118);
    let child = p.fork();
    // The child maps it again, and writes through that mapping
    p.sys(222, &[I(0), I(4096), I(3), I(1), I(5), I(0)]).save(0x120);
    p.sys(63, &[I(3), X(0x120), I(11)]).expect(11);
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), D(0x300), I(0), I(0)]);
    p.arg(0, W(0x300));
    p.expect(0); // the child's checks passed
    p.sys(64, &[I(1), X(0x118), I(11)]).expect(11);
    p.sys(215, &[X(0x118), I(4096)]).expect(0); // munmap
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"in the file");
}

#[test]
fn chown_gives_a_file_away_as_root_and_not_otherwise() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/owned\0");
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o101), I(0o644)]).expect(3);
    // fchownat(AT_FDCWD, path, 70, 70, 0), as install -o does
    p.sys(54, &[I(0xffff_ff9c), D(0x200), I(70), I(70), I(0)]).expect(0);
    p.sys(80, &[I(3), D(0x400)]).expect(0);
    p.arg(0, W(0x400 + 24));
    p.expect(70);
    // As another user: not to someone else
    p.sys(147, &[I(80), I(80), I(80)]).expect(0);
    p.sys(55, &[I(3), I(80), I(0xffff_ffff)]).expect(EPERM);
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn proc_self_fd_says_what_each_descriptor_is_open_on() {
    // realpath in Zig, Bun and Deno: open the path, then read this link
    let mut p = Prog::default();
    p.put(0x200, b"/usr/lib/real/prog\0").put(0x220, b"/usr/bin\0");
    p.put(0x240, b"/proc/self/fd/3\0").put(0x260, b"/proc/self/fd/4\0");
    p.put(0x280, b"/proc/self/fd/5\0");
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0), I(0)]).expect(3); // the file
    p.sys(56, &[I(0xffff_ff9c), D(0x220), I(0o200000), I(0)]).expect(4); // O_DIRECTORY
    p.sys(59, &[D(0x100), I(0)]).expect(0); // a pipe: 5, 6
    p.sys(78, &[I(0xffff_ff9c), D(0x240), D(0x400), I(64)]).expect(18);
    p.out(0x400, 18);
    p.sys(78, &[I(0xffff_ff9c), D(0x260), D(0x400), I(64)]).expect(8);
    p.out(0x400, 8);
    p.sys(78, &[I(0xffff_ff9c), D(0x280), D(0x400), I(64)]).expect(8);
    p.out(0x400, 5); // "pipe:", then its number
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"/usr/lib/real/prog/usr/binpipe:");
}

#[test]
fn a_pidfd_becomes_readable_when_its_process_ends_and_waitid_takes_it() {
    // The Supabase CLI watches its stack's owner with one
    let mut p = Prog::default();
    // struct pollfd for descriptor 3, POLLIN
    p.put(0x200, &3u32.to_le_bytes()).put(0x204, &1u16.to_le_bytes());
    let child = p.fork();
    p.sys(124, &[]); // the child: a yield, then it ends
    p.parent(child);
    p.save(0x110); // the child's pid
    p.sys(434, &[W(0x110), I(0)]).expect(3); // pidfd_open
    p.sys(424, &[I(3), I(0), I(0), I(0)]).expect(0); // signal 0: it is there
    // ppoll(fds, 1, NULL, NULL): until the child has ended
    p.sys(73, &[D(0x200), I(1), I(0), I(0), I(8)]).expect(1);
    p.arg(0, W(0x204));
    p.code.push(0xd350_fc00); // lsr x0, x0, #16: revents
    p.expect(1); // POLLIN
    // waitid(P_PIDFD, 3, info, WEXITED)
    p.sys(95, &[I(3), I(3), D(0x300), I(4), I(0)]).expect(0);
    p.sys(434, &[W(0x110), I(0)]).expect(-3); // ESRCH: it is gone
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

/// A machine with `elf` as /usr/lib/real/prog (linked from /usr/bin/prog) and `script`, a `#!`
/// script naming it, as /usr/bin/script
fn machine_with_script(elf: &[u8], script: &[u8]) -> Vm<armless::platform::NativePlatform> {
    let config = Config {
        memory_size: 128 << 20,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let vm = armless::runner::new_vm(config).unwrap().0;
    let json = format!(
        r#"{{"version":3,"fsroot":[
            ["usr",0,0,16877,0,0,[
                ["lib",0,0,16877,0,0,[["real",0,0,16877,0,0,[["prog",{},0,33261,0,0,"prog-blob"]]]]],
                ["bin",0,0,16877,0,0,[["prog",16,0,41471,0,0,"../lib/real/prog"],["script",{},0,33261,0,0,"script-blob"]]]]],
            ["tmp",0,0,17407,0,0,[]]]}}"#,
        elf.len(),
        script.len()
    );
    vm.board.load_filesystem(json.as_bytes()).unwrap();
    vm.board.provide_raw_blob("prog-blob", elf.to_vec()).unwrap();
    vm.board.provide_raw_blob("script-blob", script.to_vec()).unwrap();
    vm
}

#[test]
fn the_first_program_may_be_a_script_run_with_its_interpreter() {
    // An image's command is often one (Postgres's start-postgres): its interpreter gets its
    // argument, the script's path and what the script was given
    let mut p = Prog::default();
    p.code.push(0xf940_03e0); // ldr x0, [sp]: argc
    p.expect(4);
    let mut vm = machine_with_script(&p.elf(), b"#!/usr/bin/prog an-arg\nnot read\n");
    let board = vm.board.clone();
    let mut proc =
        Process::exec(&mut vm.cpus[0], board, &["/usr/bin/script".into(), "x".into()], &[], "/").unwrap();
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn posix_shared_memory_is_made_in_dev_shm() {
    // shm_open: /dev/shm/NAME, which a machine with no kernel has too (Supabase's Postgres)
    let mut p = Prog::default();
    p.put(0x200, b"/dev/shm/PostgreSQL.1\0");
    // openat(AT_FDCWD, path, O_RDWR | O_CREAT | O_EXCL, 0600), then fallocate it 2 KiB
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o302), I(0o600)]).expect(3);
    p.sys(47, &[I(3), I(0), I(0), I(2048)]).expect(0);
    p.sys(80, &[I(3), D(0x400)]).expect(0);
    p.arg(0, X(0x400 + 48)); // st_size
    p.expect(2048);
    // KEEP_SIZE reserves nothing more and changes nothing; a range past it with mode 0 grows it
    p.sys(47, &[I(3), I(1), I(0), I(8192)]).expect(0);
    p.sys(47, &[I(3), I(0), I(2048), I(1)]).expect(0);
    p.sys(80, &[I(3), D(0x400)]).expect(0);
    p.arg(0, X(0x400 + 48));
    p.expect(2049);
    p.sys(35, &[I(0xffff_ff9c), D(0x200), I(0)]).expect(0); // unlinkat: shm_unlink
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn a_read_of_an_id_register_is_answered_as_linux_does() {
    // Go's x/sys/cpu reads ID_AA64ISAR0_EL1 at start (HWCAP_CPUID): EL0 can't, the kernel answers
    let mut p = Prog::default();
    p.code.push(0xd538_0600); // mrs x0, ID_AA64ISAR0_EL1
    p.code.push(0xb500_0000 | 4 << 5); // cbnz x0, +16: past the exit below
    p.sys(94, &[I(99)]);
    p.code.push(0xd538_0000); // mrs x0, MIDR_EL1
    p.code.push(0xb500_0000 | 4 << 5);
    p.sys(94, &[I(98)]);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn a_timerfd_counts_its_expirations_and_is_readable_when_one_is_in() {
    // GHC's runtime (Supabase's PostgREST) makes one at start
    let mut p = Prog::default();
    // struct itimerspec: every 10 ms, first in 10 ms
    p.put(0x200, &[0u8; 32]).put(0x208, &10_000_000u64.to_le_bytes()).put(0x218, &10_000_000u64.to_le_bytes());
    // struct pollfd for 3, POLLIN
    p.put(0x300, &3u32.to_le_bytes()).put(0x304, &1u16.to_le_bytes());
    p.sys(85, &[I(1), I(0o4000)]).expect(3); // timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK)
    p.sys(63, &[I(3), D(0x400), I(8)]).expect(-11); // EAGAIN: disarmed
    p.sys(86, &[I(3), I(0), D(0x200), I(0)]).expect(0); // timerfd_settime
    p.sys(87, &[I(3), D(0x240)]).expect(0); // timerfd_gettime: the interval
    p.arg(0, X(0x248));
    p.code.push(0xb500_0000 | 4 << 5); // cbnz x0, +16
    p.sys(94, &[I(99)]);
    // ppoll until it fires, then a read gives how many times it did
    p.sys(73, &[D(0x300), I(1), I(0), I(0), I(8)]).expect(1);
    p.sys(63, &[I(3), D(0x400), I(8)]).expect(8);
    p.arg(0, X(0x400));
    p.code.push(0xb500_0000 | 4 << 5); // cbnz x0, +16: at least one
    p.sys(94, &[I(98)]);
    p.sys(63, &[I(3), D(0x400), I(8)]).expect(-11); // read: none since
    // Disarmed: a zero value
    p.put(0x280, &[0u8; 32]);
    p.sys(86, &[I(3), I(0), D(0x280), I(0)]).expect(0);
    p.sys(87, &[I(3), D(0x240)]).expect(0);
    p.arg(0, X(0x258));
    p.expect(0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}
