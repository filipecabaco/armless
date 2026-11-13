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
        cpus: test_cpus(),
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
    let mut vm = machine_with_cpus(1);
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

/// `ldrb w11, [x9]` for the address saved at `at`, checked to be `byte`
fn expect_byte(p: &mut Prog, at: u32, byte: u8) {
    p.arg(9, X(at));
    p.code.push(0x3940_012b);
    p.code.push(0xaa0b_03e0); // mov x0, x11
    p.expect(byte as i64);
}

/// `strb w10, [x9]`: the address saved at `at` gets `byte`
fn store_byte(p: &mut Prog, at: u32, byte: u8) {
    p.arg(9, X(at));
    p.li(10, byte as u64);
    p.code.push(0x3900_012a);
}

/// Waits for the host: a read of two bytes of the terminal, answered by `input(b"x\n")`
fn stop(p: &mut Prog) {
    p.sys(63, &[I(0), D(0x1500), I(2)]).expect(2);
}

#[test]
fn private_file_pages_are_one_frame_until_written() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0").put(0x300, &[b'A'; 4096]).put(0x1400, b"C");
    // The file, one page of As, mapped MAP_PRIVATE twice
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o600)]).expect(3);
    p.sys(64, &[I(3), D(0x300), I(4096)]); // 4096 does not fit a check
    p.sys(222, &[I(0), I(4096), I(3), I(2), I(3), I(0)]).save(0x100);
    p.sys(222, &[I(0), I(4096), I(3), I(2), I(3), I(0)]).save(0x108);
    stop(&mut p); // 0: mapped, nothing touched
    // The first mapping's page is read into a frame; the second's is that frame
    expect_byte(&mut p, 0x100, b'A');
    stop(&mut p); // 1
    expect_byte(&mut p, 0x108, b'A');
    stop(&mut p); // 2
    // A write through the first is its own copy: the second still has the file's bytes
    store_byte(&mut p, 0x100, b'B');
    expect_byte(&mut p, 0x100, b'B');
    expect_byte(&mut p, 0x108, b'A');
    stop(&mut p); // 3
    // pwrite(fd, "C", 1, 0): a mapping made after it reads the new bytes (the cached page is for
    // the old ones), and the second one, made before, goes on with what it has
    p.sys(68, &[I(3), D(0x1400), I(1), I(0)]).expect(1);
    p.sys(222, &[I(0), I(4096), I(3), I(2), I(3), I(0)]).save(0x110);
    expect_byte(&mut p, 0x110, b'C');
    expect_byte(&mut p, 0x108, b'A');
    stop(&mut p); // 4
    for at in [0x100, 0x108, 0x110] {
        p.sys(215, &[X(at), I(4096)]).expect(0);
    }
    stop(&mut p); // 5

    let mut vm = machine_with_prog(&p.elf());
    let board = vm.board.clone();
    let mut proc = Process::exec(&mut vm.cpus[0], board, &["/usr/bin/prog".into()], &[], "/").unwrap();
    let mut frames = Vec::new();
    for _ in 0..6 {
        let begun = std::time::Instant::now();
        loop {
            let status = proc.run_on(&mut vm.cpus, std::time::Duration::from_millis(20));
            if matches!(status, armless_user::Status::Idle(_)) {
                break;
            }
            assert!(matches!(status, armless_user::Status::Running), "{status:?}");
            assert!(begun.elapsed() < std::time::Duration::from_secs(10), "still running");
        }
        frames.push(proc.frames_in_use() as i64);
        proc.input(b"x\n");
    }
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
    // Reading a page costs a frame (and the page table's); a second mapping of it costs none, a
    // write its copy
    assert_eq!(frames[2], frames[1], "the second mapping shares the first's frame: {frames:?}");
    assert_eq!(frames[3] - frames[2], 1, "the write copies the page: {frames:?}");
    // The cached frame still holds the old bytes for the second mapping, so a mapping made after
    // the file changed gets a frame of its own
    assert_eq!(frames[4] - frames[3], 1, "new bytes, new frame: {frames:?}");
    // Unmapped, all of it is back: the copy, the old and new file pages (the page cache keeps none
    // of them) and the page table
    assert_eq!(frames[5], frames[0], "nothing is kept once unmapped: {frames:?}");
}

/// The frames in use at each of the `stops` of the program `p`
fn frames_at_stops(p: Prog, stops: usize) -> Vec<i64> {
    let mut vm = machine_with_prog(&p.elf());
    let board = vm.board.clone();
    let mut proc = Process::exec(&mut vm.cpus[0], board, &["/usr/bin/prog".into()], &[], "/").unwrap();
    let mut frames = Vec::new();
    for _ in 0..stops {
        let begun = std::time::Instant::now();
        loop {
            let status = proc.run_on(&mut vm.cpus, std::time::Duration::from_millis(20));
            if matches!(status, armless_user::Status::Idle(_)) {
                break;
            }
            assert!(matches!(status, armless_user::Status::Running), "{status:?}");
            assert!(begun.elapsed() < std::time::Duration::from_secs(10), "still running");
        }
        frames.push(proc.frames_in_use() as i64);
        proc.input(b"x\n");
    }
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
    frames
}

#[test]
fn a_write_after_the_file_changed_still_lets_go_of_the_cached_frame() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0").put(0x300, &[b'A'; 4096]).put(0x1400, b"C");
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o600)]).expect(3);
    p.sys(64, &[I(3), D(0x300), I(4096)]);
    p.sys(222, &[I(0), I(4096), I(3), I(2), I(3), I(0)]).save(0x100);
    // The page is read into a frame, which the page cache holds too
    expect_byte(&mut p, 0x100, b'A');
    stop(&mut p); // 0
    // The file changes, then the page is written: the copy is the mapping's, and the cached frame
    // (cached for the file as it was) has no one to serve any more
    p.sys(68, &[I(3), D(0x1400), I(1), I(0)]).expect(1);
    store_byte(&mut p, 0x100, b'B');
    stop(&mut p); // 1
    let frames = frames_at_stops(p, 2);
    assert_eq!(frames[1], frames[0], "the old frame went with the copy, not at the next unmap: {frames:?}");
}

#[test]
fn a_fixed_mapping_over_file_pages_lets_go_of_their_cached_frames() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0").put(0x300, &[b'A'; 4096]);
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o600)]).expect(3);
    for _ in 0..6 {
        p.sys(64, &[I(3), D(0x300), I(4096)]);
    }
    // An address to map at: the last page of the file, not read
    p.sys(222, &[I(0), I(4096), I(3), I(2), I(3), I(5 * 4096)]).save(0x100);
    // A different page of the file at that address (MAP_FIXED | MAP_PRIVATE) each time, each read
    for i in 0..5u64 {
        p.sys(222, &[X(0x100), I(4096), I(3), I(0x12), I(3), I(i * 4096)]);
        expect_byte(&mut p, 0x100, b'A');
        stop(&mut p); // i
    }
    let frames = frames_at_stops(p, 5);
    assert!(frames.iter().all(|&n| n == frames[0]), "each page replaced is let go, not kept until an unmap: {frames:?}");
}

#[test]
fn shrinking_a_file_mapping_lets_go_of_the_cached_frames_of_what_is_cut() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0").put(0x240, b"/tmp/g\0").put(0x300, &[b'A'; 4096]);
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o600)]).expect(3);
    p.sys(56, &[I(0xffff_ff9c), D(0x240), I(0o102), I(0o600)]).expect(4);
    for _ in 0..3 {
        p.sys(64, &[I(3), D(0x300), I(4096)]);
    }
    p.sys(222, &[I(0), I(3 * 4096), I(3), I(2), I(3), I(0)]).save(0x100);
    // The host reads the three pages of the mapping (to write them to the other file)
    p.sys(64, &[I(4), X(0x100), I(3 * 4096)]);
    stop(&mut p); // 0
    // mremap(addr, 3 pages, 1 page, 0): two pages of it are cut
    p.sys(216, &[X(0x100), I(3 * 4096), I(4096), I(0)]);
    stop(&mut p); // 1
    let frames = frames_at_stops(p, 2);
    assert_eq!(frames[0] - frames[1], 2, "the cut pages' frames are not kept by the cache: {frames:?}");
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
        cpus: test_cpus(),
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

// ------------------------------------------------------------ groups, AIO, preadv2 and pwritev2

const EBADF: i64 = -9;
const EINVAL: i64 = -22;
const EOPNOTSUPP: i64 = -95;
/// openat's AT_FDCWD, and O_RDWR | O_CREAT
const AT_FDCWD: u64 = 0xffff_ff9c;
const O_RDWR_CREAT: u64 = 0o102;
const O_APPEND: u64 = 0o2000;

/// struct iocb: the request `op` on `fd` of `n` bytes at `buf` (an offset of the data), at `off`,
/// with the RWF_ `rw_flags`, and an eventfd to signal
#[allow(clippy::too_many_arguments)] // struct iocb's fields
fn iocb(data: u64, op: u16, fd: u32, buf: u32, n: u64, off: u64, rw_flags: u32, resfd: Option<u32>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&data.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // aio_key
    b.extend_from_slice(&rw_flags.to_le_bytes());
    b.extend_from_slice(&op.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // aio_reqprio
    b.extend_from_slice(&fd.to_le_bytes());
    // No bytes at offset 0 is no buffer: NULL (an FSYNC has none)
    let at = if buf == 0 && n == 0 { 0 } else { DATA + buf as u64 };
    b.extend_from_slice(&at.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&off.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // aio_reserved2
    b.extend_from_slice(&(resfd.is_some() as u32).to_le_bytes()); // IOCB_FLAG_RESFD
    b.extend_from_slice(&resfd.unwrap_or(0).to_le_bytes());
    b
}

/// The address of the data's offset `off`, as a u64 in memory (an iocb pointer, an iovec base)
fn addr(off: u32) -> [u8; 8] {
    (DATA + off as u64).to_le_bytes()
}

/// iovecs at 0x700 on: "abc", "def", "XY", "Z" (put at 0x220 on), and 16 bytes at 0x400 to read into
fn iovecs(p: &mut Prog) {
    p.put(0x220, b"abcdefXYZ");
    for (i, (at, n)) in [(0x220, 3u64), (0x223, 3), (0x226, 2), (0x228, 1), (0x400, 16)].into_iter().enumerate() {
        p.put(0x700 + 16 * i as u32, &addr(at)).put(0x708 + 16 * i as u32, &n.to_le_bytes());
    }
}
const ABC: u32 = 0x700;
const XY: u32 = 0x720;
const Z: u32 = 0x730;
const INTO: u32 = 0x740;

/// preadv2 (286) or pwritev2 (287) on `fd` of `cnt` iovecs at `iov`, at `pos` or (None, -1) the
/// descriptor's position
fn rwv2(p: &mut Prog, nr: u64, fd: u64, iov: u32, cnt: u64, pos: Option<u64>, flags: u64) -> &mut Prog {
    p.arg(0, I(fd));
    p.arg(1, D(iov));
    p.arg(2, I(cnt));
    match pos {
        Some(at) => p.arg(3, I(at)),
        None => p.code.push(0x9280_0003), // movn x3, #0: -1
    }
    p.arg(4, I(0));
    p.arg(5, I(flags));
    p.li(8, nr);
    p.code.push(0xd400_0001);
    p
}

/// lseek(fd, 0, SEEK_CUR): the descriptor's position
fn pos(p: &mut Prog, fd: u64) -> &mut Prog {
    p.sys(62, &[I(fd), I(0), I(1)])
}

#[test]
fn setgroups_and_getgroups_keep_a_list_a_child_inherits() {
    let mut p = Prog::default();
    p.put(0x200, &5u32.to_le_bytes()).put(0x204, &7u32.to_le_bytes());
    p.sys(158, &[I(0), I(0)]).expect(0); // getgroups: none to start with
    p.sys(159, &[I(2), D(0x200)]).expect(0);
    p.sys(158, &[I(0), I(0)]).expect(2); // size 0: how many
    p.sys(158, &[I(1), D(0x300)]).expect(EINVAL); // too small
    p.sys(158, &[I(2), D(0x300)]).expect(2);
    p.arg(0, W(0x300));
    p.expect(5);
    p.arg(0, W(0x304));
    p.expect(7);
    p.sys(159, &[I(65537), D(0x200)]).expect(EINVAL); // more than NGROUPS_MAX
    p.sys(159, &[I(1), I(0)]).expect(-14); // EFAULT
    p.sys(158, &[I(0), I(0)]).expect(2); // the failures changed nothing
    let child = p.fork();
    // The child has them, and empties its own list
    p.sys(158, &[I(0), I(0)]).expect(2);
    p.sys(159, &[I(0), I(0)]).expect(0);
    p.sys(158, &[I(0), I(0)]).expect(0);
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), D(0x380), I(0), I(0)]);
    p.arg(0, W(0x380));
    p.expect(0); // the child's checks passed
    p.sys(158, &[I(0), I(0)]).expect(2); // the parent's are its own
    // Not root any more: no groups to set, and those it has stay
    p.sys(147, &[I(70), I(70), I(70)]).expect(0);
    p.sys(159, &[I(0), I(0)]).expect(EPERM);
    p.sys(158, &[I(0), I(0)]).expect(2);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn aio_writes_and_reads_a_file_and_signals_the_eventfd() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0").put(0x220, b"hello");
    // PWRITE "hello" at 0 with the eventfd; PREAD 5 at 0, and at 3; a bad opcode; a bad fd
    p.put(0x500, &iocb(0x111, 1, 3, 0x220, 5, 0, 0, Some(4)));
    p.put(0x540, &iocb(0x222, 0, 3, 0x400, 5, 0, 0, None));
    p.put(0x580, &iocb(0x333, 0, 3, 0x420, 5, 3, 0, None));
    p.put(0x5c0, &iocb(0x444, 9, 3, 0x400, 5, 0, 0, None));
    p.put(0x600, &iocb(0x555, 0, 40, 0x400, 5, 0, 0, None));
    for (i, at) in [0x500, 0x540, 0x580, 0x5c0, 0x600].into_iter().enumerate() {
        p.put(0x108 + 8 * i as u32, &addr(at));
    }
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_RDWR_CREAT), I(0o644)]).expect(3);
    p.sys(19, &[I(0), I(0)]).expect(4); // eventfd2
    p.sys(0, &[I(8), D(0x100)]).expect(0); // io_setup: the context at 0x100
    p.sys(0, &[I(8), D(0x100)]).expect(EINVAL); // *ctxp must be 0
    p.sys(2, &[X(0x100), I(1), D(0x108)]).expect(1); // io_submit
    p.sys(4, &[X(0x100), I(1), I(1), D(0x300), I(0)]).expect(1); // io_getevents
    p.arg(0, W(0x300)); // data
    p.expect(0x111);
    p.arg(0, W(0x310)); // res
    p.expect(5);
    // The eventfd counted the completion
    p.sys(63, &[I(4), D(0x380), I(8)]).expect(8);
    p.arg(0, W(0x380));
    p.expect(1);
    // Both reads at once: 5 bytes at 0, the 2 left at 3
    p.sys(2, &[X(0x100), I(2), D(0x110)]).expect(2);
    p.sys(4, &[X(0x100), I(2), I(8), D(0x300), I(0)]).expect(2);
    p.arg(0, W(0x310));
    p.expect(5);
    p.arg(0, W(0x330));
    p.expect(2);
    p.out(0x400, 5);
    p.out(0x420, 2);
    // Nothing to cancel; nothing queued, and a zero timeout (at 0x340) returns at once
    p.sys(3, &[X(0x100), D(0x540), D(0x300)]).expect(EINVAL);
    p.sys(4, &[X(0x100), I(1), I(1), D(0x300), D(0x340)]).expect(0);
    p.sys(2, &[X(0x100), I(1), D(0x120)]).expect(EINVAL); // the bad opcode
    p.sys(2, &[X(0x100), I(1), D(0x128)]).expect(EBADF);
    // A child doesn't have its parent's contexts
    let child = p.fork();
    p.sys(2, &[X(0x100), I(1), D(0x110)]).expect(EINVAL);
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), D(0x380), I(0), I(0)]);
    p.arg(0, W(0x380));
    p.expect(0);
    // io_destroy, and the context is gone
    p.sys(1, &[X(0x100)]).expect(0);
    p.sys(1, &[X(0x100)]).expect(EINVAL);
    p.sys(4, &[X(0x100), I(0), I(1), D(0x300), I(0)]).expect(EINVAL);
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"hellolo");
}

#[test]
fn aio_getevents_waits_its_timeout_and_then_gives_the_events_there_are() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0").put(0x220, b"hello");
    p.put(0x360, &10_000_000u64.to_le_bytes()); // a timespec of 10 ms at 0x358
    p.put(0x500, &iocb(0x111, 1, 3, 0x220, 5, 0, 0, None));
    p.put(0x108, &addr(0x500));
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_RDWR_CREAT), I(0o644)]).expect(3);
    p.sys(0, &[I(8), D(0x100)]).expect(0);
    // Nothing queued: none when the time is up
    p.sys(4, &[X(0x100), I(1), I(1), D(0x300), D(0x358)]).expect(0);
    // One completion, and two wanted: the one, when the time is up (io_pgetevents as well)
    p.sys(2, &[X(0x100), I(1), D(0x108)]).expect(1);
    p.sys(292, &[X(0x100), I(2), I(2), D(0x300), D(0x358), I(0)]).expect(1);
    p.arg(0, W(0x300));
    p.expect(0x111);
    p.arg(0, W(0x310));
    p.expect(5);
    // It was taken: none left
    p.sys(4, &[X(0x100), I(1), I(1), D(0x300), D(0x358)]).expect(0);
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn aio_contexts_and_groups_survive_a_snapshot() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &5u32.to_le_bytes()).put(0x204, &7u32.to_le_bytes());
    // An FSYNC of stdout, whose completion waits across the snapshot
    p.put(0x500, &iocb(0x222, 2, 1, 0, 0, 0, 0, None)).put(0x108, &addr(0x500));
    p.sys(159, &[I(2), D(0x200)]).expect(0);
    p.sys(0, &[I(4), D(0x100)]).expect(0);
    p.sys(2, &[X(0x100), I(1), D(0x108)]).expect(1);
    p.sys(63, &[I(0), D(0x380), I(1)]).expect(1); // waits for a key
    p.sys(158, &[I(2), D(0x300)]).expect(2);
    p.arg(0, W(0x304));
    p.expect(7);
    p.sys(4, &[X(0x100), I(1), I(1), D(0x340), I(0)]).expect(1);
    p.arg(0, W(0x340));
    p.expect(0x222);
    let mut p = start(&mut vm, p);
    let status = loop {
        let status = p.run_on(&mut vm.cpus, std::time::Duration::from_millis(20));
        if !matches!(status, armless_user::Status::Running) {
            break status;
        }
    };
    assert!(matches!(status, armless_user::Status::Idle(None)), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], Some(vm.board.clone()), &process).unwrap();
    p.input(b"\n");
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn pwritev2_takes_noappend_and_dontcache_and_not_atomic() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0");
    iovecs(&mut p);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_RDWR_CREAT | O_APPEND), I(0o644)]).expect(3);
    // "abc" appended, then back to the start
    rwv2(&mut p, 287, 3, ABC, 1, None, 0).expect(3);
    p.sys(62, &[I(3), I(0), I(0)]).expect(0);
    // RWF_NOAPPEND at the descriptor's position (0): "XY" over "ab", and the position moves on
    rwv2(&mut p, 287, 3, XY, 1, None, 0x20).expect(2);
    pos(&mut p, 3).expect(2);
    // RWF_NOAPPEND at 1: "Z" over "Y", and the position stays
    rwv2(&mut p, 287, 3, Z, 1, Some(1), 0x20).expect(1);
    pos(&mut p, 3).expect(2);
    // Without it O_APPEND sends the write to the end
    rwv2(&mut p, 287, 3, ABC + 16, 1, None, 0).expect(3);
    // RWF_DONTCACHE changes nothing, on a write or a read
    rwv2(&mut p, 287, 3, Z, 1, Some(0), 0xa0).expect(1);
    rwv2(&mut p, 286, 3, INTO, 1, Some(0), 0x80).expect(6);
    p.out(0x400, 6);
    // RWF_ATOMIC is not for this filesystem; APPEND and NOAPPEND together make no sense
    rwv2(&mut p, 287, 3, Z, 1, Some(0), 0x40).expect(EOPNOTSUPP);
    rwv2(&mut p, 287, 3, Z, 1, Some(0), 0x30).expect(EINVAL);
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"ZZcdef");
}

#[test]
fn a_positioned_write_to_a_file_opened_for_appending_goes_to_its_end() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0");
    iovecs(&mut p);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_RDWR_CREAT | O_APPEND), I(0o644)]).expect(3);
    p.sys(64, &[I(3), D(0x220), I(3)]).expect(3); // "abc": the position is 3
    // pwrite64 "Z" at 0 goes to the end, and the position stays
    p.sys(68, &[I(3), D(0x228), I(1), I(0)]).expect(1);
    pos(&mut p, 3).expect(3);
    // So do pwritev and pwritev2
    p.sys(70, &[I(3), D(Z), I(1), I(0), I(0)]).expect(1);
    pos(&mut p, 3).expect(3);
    rwv2(&mut p, 287, 3, Z, 1, Some(0), 0).expect(1);
    p.sys(67, &[I(3), D(0x400), I(16), I(0)]).expect(6);
    p.out(0x400, 6);
    // With RWF_NOAPPEND it writes where it is told
    rwv2(&mut p, 287, 3, Z, 1, Some(0), 0x20).expect(1);
    p.sys(67, &[I(3), D(0x400), I(16), I(0)]).expect(6);
    p.out(0x400, 6);
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"abcZZZZbcZZZ");
}

#[test]
fn preadv2_and_pwritev2_take_a_position_or_the_descriptors_and_their_flags() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0");
    iovecs(&mut p);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_RDWR_CREAT), I(0o644)]).expect(3);
    // "abc" "def" at the descriptor's position: 6 bytes, and "XY" at 1 leaves the position at 6
    rwv2(&mut p, 287, 3, ABC, 2, None, 0).expect(6);
    rwv2(&mut p, 287, 3, XY, 1, Some(1), 0).expect(2);
    rwv2(&mut p, 286, 3, INTO, 1, None, 0).expect(0); // at the end: nothing
    rwv2(&mut p, 287, 3, XY, 1, Some(0), 0x100).expect(EOPNOTSUPP); // a flag it doesn't know
    // RWF_DSYNC is a plain write; RWF_APPEND goes to the end even at a position, which stays
    rwv2(&mut p, 287, 3, XY, 1, Some(0), 2).expect(2);
    rwv2(&mut p, 287, 3, Z, 1, Some(0), 0x10).expect(1);
    pos(&mut p, 3).expect(6);
    // RWF_APPEND means nothing to a read (Linux takes it)
    rwv2(&mut p, 286, 3, INTO, 1, Some(0), 0x10).expect(7);
    p.out(0x400, 7);
    // At the descriptor's position RWF_APPEND moves it to the new end
    rwv2(&mut p, 287, 3, Z, 1, None, 0x10).expect(1);
    pos(&mut p, 3).expect(8);
    // RWF_NOWAIT: a file's bytes are there; an empty pipe's read would wait, so EAGAIN
    rwv2(&mut p, 286, 3, INTO, 1, Some(0), 8).expect(8);
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 4, 5
    rwv2(&mut p, 286, 4, INTO, 1, None, 8).expect(EAGAIN);
    let (out, status) = run_linked(p);
    ok(&status);
    assert_eq!(out, b"XYYdefZ");
}

// ---------------------------------------------------------------- timers

const SIGVTALRM: u64 = 26;
const SIGPROF: u64 = 27;
const SI_KERNEL: i64 = 0x80;
const SI_TIMER: i64 = -2;
const CLOCK_MONOTONIC: u64 = 1;
const SIGEV_SIGNAL: u32 = 0;
const SIGEV_NONE: u32 = 1;
const SIGEV_THREAD: u32 = 2;
const SIGEV_THREAD_ID: u32 = 4;
const TIMER_ABSTIME: u64 = 1;
/// Conditions of b.cond
const NE: u32 = 1;
const HS: u32 = 2;
const LO: u32 = 3;

/// A signal mask of `sigs`
fn mask(sigs: &[u64]) -> [u8; 8] {
    sigs.iter().fold(0u64, |m, s| m | 1 << (s - 1)).to_le_bytes()
}

/// struct itimerval or itimerspec: {interval, value}, each {seconds, us or ns}
fn pair(interval: (u64, u64), value: (u64, u64)) -> Vec<u8> {
    [interval.0, interval.1, value.0, value.1]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

/// struct sigevent: sigev_value, sigev_signo, sigev_notify (the thread for SIGEV_THREAD_ID at
/// 16, put there by the program)
fn sigevent(value: u64, sig: u64, notify: u32) -> Vec<u8> {
    let mut b = value.to_le_bytes().to_vec();
    b.extend_from_slice(&(sig as u32).to_le_bytes());
    b.extend_from_slice(&notify.to_le_bytes());
    b.resize(64, 0);
    b
}

/// Exits with 100 + the check's number unless x0 `cond` `v` (unsigned)
fn check(p: &mut Prog, cond: u32, v: u64) {
    p.checks += 1;
    p.li(1, v);
    p.code.push(0xeb01_001f); // cmp x0, x1
    p.code.push(0x5400_0080 | cond); // b.cond +16
    p.li(0, 100 + p.checks as u64);
    p.li(8, 94);
    p.code.push(0xd400_0001);
}

/// x0 = the signed 32-bit value at this offset of the data (a siginfo's si_code)
fn signed(p: &mut Prog, off: u32) {
    p.arg(0, W(off));
    p.code.push(0x9340_7c00); // sxtw x0, w0
}

#[test]
fn the_interval_timer_signals_at_every_interval_and_is_quiet_once_disarmed() {
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGALRM]));
    p.put(0x200, &pair((0, 10_000), (0, 10_000))).put(0x220, &[0; 32]);
    p.put(0x240, &pair((0, 40_000_000), (0, 0))); // a 40 ms timespec
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(103, &[I(0), D(0x200), I(0)]).expect(0);
    // rt_sigtimedwait: each comes from the kernel
    for _ in 0..3 {
        p.sys(137, &[D(0x100), D(0x400), I(0), I(8)]).expect(SIGALRM as i64);
        signed(&mut p, 0x408);
        p.expect(SI_KERNEL);
    }
    p.sys(103, &[I(0), D(0x220), I(0)]).expect(0);
    p.sys(137, &[D(0x100), I(0), D(0x240), I(8)]).expect(EAGAIN);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn getitimer_counts_down_and_setitimer_gives_back_the_old_timer() {
    let mut p = Prog::default();
    p.put(0x200, &pair((0, 7_000), (0, 40_000))).put(0x220, &[0; 32]);
    p.put(0x240, &pair((0, 5_000_000), (0, 0))); // a 5 ms timespec
    p.put(0x260, &pair((0, 0), (0, 1_000_000)));
    // ITIMER_VIRTUAL counts CPU time, of which the program has used little: it is armed
    p.sys(103, &[I(1), D(0x200), I(0)]).expect(0);
    p.sys(102, &[I(1), D(0x300)]).expect(0);
    p.arg(0, X(0x318));
    check(&mut p, NE, 0);
    p.sys(103, &[I(1), D(0x220), I(0)]).expect(0);
    // ITIMER_REAL, 5 ms on: 0 < what is left < 40 ms, and the interval is the one set
    p.sys(103, &[I(0), D(0x200), I(0)]).expect(0);
    p.sys(101, &[D(0x240), I(0)]).expect(0);
    p.sys(102, &[I(0), D(0x300)]).expect(0);
    p.arg(0, X(0x300));
    p.expect(0);
    p.arg(0, X(0x308));
    check(&mut p, 0, 7_000);
    p.arg(0, X(0x310));
    p.expect(0);
    p.arg(0, X(0x318));
    check(&mut p, NE, 0);
    p.arg(0, X(0x318));
    check(&mut p, LO, 40_000);
    // The old timer comes back from the next setitimer, which disarms it
    p.sys(103, &[I(0), D(0x220), D(0x340)]).expect(0);
    p.arg(0, X(0x358));
    check(&mut p, NE, 0);
    p.arg(0, X(0x358));
    check(&mut p, LO, 40_000);
    p.sys(102, &[I(0), D(0x300)]).expect(0);
    p.arg(0, X(0x318));
    p.expect(0);
    // A bad timer, or a microsecond count of a million, is EINVAL
    p.sys(103, &[I(0), D(0x260), I(0)]).expect(EINVAL);
    p.sys(103, &[I(3), D(0x220), I(0)]).expect(EINVAL);
    p.sys(102, &[I(3), D(0x300)]).expect(EINVAL);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn the_cpu_time_interval_timers_fire_as_the_program_runs() {
    // ITIMER_VIRTUAL and ITIMER_PROF at 1 ms of CPU time: a program that spins gets both
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGVTALRM, SIGPROF]));
    p.put(0x200, &pair((0, 0), (0, 1_000)));
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(103, &[I(1), D(0x200), I(0)]).expect(0);
    p.sys(103, &[I(2), D(0x200), I(0)]).expect(0);
    // Until rt_sigpending has both
    let top = p.code.len();
    p.sys(136, &[D(0x300), I(8)]);
    p.arg(0, X(0x300));
    p.arg(1, X(0x100));
    p.code.push(0xeb01_001f); // cmp x0, x1
    let back = (top as i32 - p.code.len() as i32) as u32 & 0x7ffff;
    p.code.push(0x5400_0000 | back << 5 | NE); // b.ne top
    // Their CPU time clocks have counted it
    p.sys(113, &[I(2), D(0x340)]).expect(0); // CLOCK_PROCESS_CPUTIME_ID
    p.arg(0, X(0x348));
    check(&mut p, NE, 0);
    p.sys(113, &[I(3), D(0x360)]).expect(0); // CLOCK_THREAD_CPUTIME_ID
    p.arg(0, X(0x368));
    check(&mut p, NE, 0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

/// A program whose POSIX timer (its second, id 1) signals SIGUSR1 as `notify` says once, after
/// 10 ms: rt_sigtimedwait has its siginfo
fn posix_timer(notify: u32) -> Prog {
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGUSR1]));
    p.put(0x200, &sigevent(0x123, SIGUSR1, notify));
    p.put(0x2a0, &pair((0, 0), (0, 10_000_000)));
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(178, &[]).save(0x210); // gettid, for SIGEV_THREAD_ID
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x280)]).expect(0);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x284)]).expect(0);
    p.arg(0, W(0x280));
    p.expect(0);
    p.arg(0, W(0x284));
    p.expect(1);
    p.sys(110, &[W(0x284), I(0), D(0x2a0), I(0)]).expect(0);
    p.sys(137, &[D(0x100), D(0x400), I(0), I(8)]).expect(SIGUSR1 as i64);
    // si_code, si_timerid, si_overrun, si_value
    signed(&mut p, 0x408);
    p.expect(SI_TIMER);
    p.arg(0, W(0x410));
    p.expect(1);
    p.arg(0, W(0x414));
    p.expect(0);
    p.arg(0, X(0x418));
    p.expect(0x123);
    p.sys(109, &[W(0x284)]).expect(0); // timer_getoverrun
    p.sys(111, &[W(0x284)]).expect(0); // timer_delete
    p.sys(110, &[W(0x284), I(0), D(0x2a0), I(0)]).expect(EINVAL); // gone
    p
}

#[test]
fn a_posix_timer_signals_with_its_id_and_value() {
    for notify in [SIGEV_SIGNAL, SIGEV_THREAD] {
        let mut vm = machine();
        let mut proc = start(&mut vm, posix_timer(notify));
        let (_, status) = run(&mut proc, &mut vm);
        ok(&status);
    }
}

#[test]
fn a_posix_timer_can_signal_a_thread() {
    // SIGEV_THREAD_ID, the way glibc's SIGEV_THREAD has it
    let mut vm = machine();
    let mut proc = start(&mut vm, posix_timer(SIGEV_THREAD_ID));
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn a_deleted_posix_timer_is_quiet_and_so_is_one_with_no_notification() {
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGUSR1]));
    p.put(0x200, &sigevent(0, SIGUSR1, SIGEV_SIGNAL));
    p.put(0x240, &pair((0, 40_000_000), (0, 0))); // a 40 ms timespec
    p.put(0x2a0, &pair((0, 0), (0, 10_000_000)));
    p.put(0x2e0, &sigevent(0, SIGUSR1, SIGEV_NONE));
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x280)]).expect(0);
    p.sys(110, &[W(0x280), I(0), D(0x2a0), I(0)]).expect(0);
    p.sys(111, &[W(0x280)]).expect(0);
    p.sys(137, &[D(0x100), I(0), D(0x240), I(8)]).expect(EAGAIN);
    // SIGEV_NONE: it runs down, and nothing is signalled
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x2e0), D(0x284)]).expect(0);
    p.sys(110, &[W(0x284), I(0), D(0x2a0), I(0)]).expect(0);
    p.sys(101, &[D(0x240), I(0)]).expect(0);
    p.sys(108, &[W(0x284), D(0x300)]).expect(0); // timer_gettime: not armed any more
    p.arg(0, X(0x310));
    p.expect(0);
    p.arg(0, X(0x318));
    p.expect(0);
    p.sys(136, &[D(0x340), I(8)]).expect(0);
    p.arg(0, X(0x340));
    p.expect(0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn a_posix_timer_counts_the_expiries_a_blocked_signal_missed() {
    // Every 5 ms for 60 ms with the signal blocked: one signal, which says how many more there
    // were; a signalfd reads it with the timer's id and value
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGUSR1]));
    p.put(0x200, &sigevent(0x123, SIGUSR1, SIGEV_SIGNAL));
    p.put(0x240, &pair((0, 60_000_000), (0, 0))); // a 60 ms timespec
    p.put(0x2a0, &pair((0, 5_000_000), (0, 5_000_000)));
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(74, &[I(0xffff_ffff), D(0x100), I(8), I(0)]).expect(3);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x280)]).expect(0);
    p.sys(110, &[W(0x280), I(0), D(0x2a0), I(0)]).expect(0);
    p.sys(101, &[D(0x240), I(0)]).expect(0);
    p.sys(63, &[I(3), D(0x400), I(128)]).expect(128);
    // ssi_code, ssi_tid (the timer), ssi_overrun, ssi_ptr
    signed(&mut p, 0x408);
    p.expect(SI_TIMER);
    p.arg(0, W(0x418));
    p.expect(0);
    p.arg(0, W(0x420));
    check(&mut p, HS, 4);
    p.arg(0, X(0x430));
    p.expect(0x123);
    p.sys(109, &[W(0x280)]);
    check(&mut p, HS, 4);
    p.sys(111, &[W(0x280)]).expect(0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn an_absolute_posix_timer_is_due_when_its_clock_reads_that_time() {
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGUSR1]));
    p.put(0x200, &sigevent(0, SIGUSR1, SIGEV_SIGNAL));
    p.put(0x2a0, &[0; 32]);
    p.sys(135, &[I(SIG_BLOCK), D(0x100), I(0), I(8)]).expect(0);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x280)]).expect(0);
    p.sys(113, &[I(CLOCK_MONOTONIC), D(0x300)]).expect(0);
    // 1000 s from now on the monotonic clock: 999 and some left
    p.arg(0, X(0x300));
    p.code.push(0x910f_a000); // add x0, x0, #1000
    p.save(0x2b0);
    p.arg(0, X(0x308));
    p.save(0x2b8);
    p.sys(110, &[W(0x280), I(TIMER_ABSTIME), D(0x2a0), I(0)]).expect(0);
    p.sys(108, &[W(0x280), D(0x340)]).expect(0);
    p.arg(0, X(0x350));
    p.expect(999);
    // A time that has gone by: due at once (and the old setting comes back)
    p.arg(0, X(0x300));
    p.save(0x2b0);
    p.sys(110, &[W(0x280), I(TIMER_ABSTIME), D(0x2a0), D(0x380)]).expect(0);
    p.arg(0, X(0x390));
    p.expect(999);
    p.sys(137, &[D(0x100), I(0), I(0), I(8)]).expect(SIGUSR1 as i64);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn a_forked_child_has_no_timers() {
    let mut p = Prog::default();
    p.put(0x200, &sigevent(0, SIGUSR1, SIGEV_SIGNAL));
    p.put(0x220, &pair((0, 0), (5, 0))).put(0x260, &[0; 32]);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x280)]).expect(0);
    p.sys(103, &[I(0), D(0x220), I(0)]).expect(0);
    let child = p.fork();
    p.sys(102, &[I(0), D(0x300)]).expect(0);
    for off in [0x300, 0x308, 0x310, 0x318] {
        p.arg(0, X(off));
        p.expect(0);
    }
    p.sys(108, &[W(0x280), D(0x340)]).expect(EINVAL); // the parent's POSIX timer isn't here
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), D(0x380), I(0), I(0)]);
    p.arg(0, W(0x380));
    p.expect(0); // the child's checks passed
    // The parent's are still there
    p.sys(102, &[I(0), D(0x300)]).expect(0);
    p.arg(0, X(0x310));
    p.expect(4);
    p.sys(108, &[W(0x280), D(0x340)]).expect(0);
    p.sys(103, &[I(0), D(0x260), I(0)]).expect(0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn execve_keeps_the_interval_timers_and_deletes_the_posix_timers() {
    // The program runs twice: first (argc 1) it arms both and execs itself with an argument;
    // then (argc 2) it looks
    let mut p = Prog::default();
    p.put(0x200, b"/usr/bin/prog\0").put(0x210, b"x\0");
    let at = |off: u64| (DATA + off).to_le_bytes();
    p.put(0x300, &at(0x200)).put(0x308, &at(0x210)).put(0x310, &[0; 8]);
    p.put(0x320, &sigevent(0, SIGUSR1, SIGEV_SIGNAL));
    p.put(0x380, &pair((0, 0), (5, 0))).put(0x3a0, &[0; 32]);
    p.code.push(0xf940_03e0); // ldr x0, [sp]: argc
    p.code.push(0xf100_041f); // cmp x0, #1
    let branch = p.code.len();
    p.code.push(0);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x320), D(0x3c0)]).expect(0);
    p.sys(103, &[I(0), D(0x380), I(0)]).expect(0);
    p.sys(221, &[D(0x200), D(0x300), D(0x310)]);
    p.sys(94, &[I(99)]);
    let skip = (p.code.len() - branch) as u32;
    p.code[branch] = 0x5400_0000 | skip << 5 | NE; // b.ne: the second run
    p.sys(102, &[I(0), D(0x400)]).expect(0);
    p.arg(0, X(0x410));
    p.expect(4);
    p.sys(108, &[I(0), D(0x440)]).expect(EINVAL);
    p.sys(103, &[I(0), D(0x3a0), I(0)]).expect(0);
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn armed_timers_go_on_in_a_restored_process() {
    // Saved while the program waits for its interval timer (60 ms), with a POSIX timer (80 ms)
    // and a CPU-time one armed: another machine restores it, and each signal comes at its time
    let mut p = Prog::default();
    p.put(0x100, &mask(&[SIGALRM])).put(0x108, &mask(&[SIGUSR1]));
    p.put(0x110, &mask(&[SIGALRM, SIGUSR1]));
    p.put(0x200, &sigevent(0x55, SIGUSR1, SIGEV_SIGNAL));
    p.put(0x260, &pair((0, 0), (0, 60_000))).put(0x280, &pair((0, 0), (100, 0)));
    p.put(0x2a0, &pair((0, 0), (0, 80_000_000)));
    p.sys(135, &[I(SIG_BLOCK), D(0x110), I(0), I(8)]).expect(0);
    p.sys(107, &[I(CLOCK_MONOTONIC), D(0x200), D(0x2f0)]).expect(0);
    p.sys(110, &[W(0x2f0), I(0), D(0x2a0), I(0)]).expect(0);
    p.sys(103, &[I(1), D(0x280), I(0)]).expect(0);
    p.sys(103, &[I(0), D(0x260), I(0)]).expect(0);
    p.sys(137, &[D(0x100), I(0), I(0), I(8)]).expect(SIGALRM as i64);
    p.sys(137, &[D(0x108), D(0x400), I(0), I(8)]).expect(SIGUSR1 as i64);
    p.arg(0, X(0x418));
    p.expect(0x55);
    p.sys(102, &[I(1), D(0x300)]).expect(0);
    p.arg(0, X(0x310));
    p.expect(99);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let status = loop {
        match proc.run_on(&mut vm.cpus, std::time::Duration::from_millis(20)) {
            armless_user::Status::Running => {},
            s => break s,
        }
    };
    // Waiting, with nothing but the timer to wake it
    assert!(matches!(status, armless_user::Status::Idle(Some(_))), "{status:?}");
    let process = proc.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((proc, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut proc = Process::restore(&mut vm.cpus[0], None, &process).unwrap();
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

/// struct flock: a lock of `typ` (0 read, 1 write, 2 unlock) on the whole file
fn flock_all(typ: u16) -> Vec<u8> {
    let mut b = vec![0u8; 32];
    b[0..2].copy_from_slice(&typ.to_le_bytes());
    b
}

#[test]
fn a_record_lock_holds_others_off_until_its_process_closes_the_file() {
    // SQLite's locks, by which the Supabase CLI tells its stack's owner is running
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/locked\0");
    p.put(0x300, &flock_all(1)).put(0x340, &flock_all(1)).put(0x380, &flock_all(1));
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o644)]).expect(3);
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(2), I(0)]).expect(4);
    p.sys(25, &[I(3), I(6), D(0x300)]).expect(0); // F_SETLK, write, the whole file
    let child = p.fork();
    // Another process: F_GETLK names the holder, F_SETLK is refused
    p.sys(25, &[I(3), I(5), D(0x340)]).expect(0);
    p.arg(0, W(0x340));
    p.expect(1); // F_WRLCK, from the start (whence 0)
    p.arg(0, W(0x340 + 24));
    p.expect(1000); // its pid
    p.sys(25, &[I(3), I(6), D(0x380)]).expect(-11); // EAGAIN
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), D(0x500), I(0), I(0)]);
    p.arg(0, W(0x500));
    p.expect(0);
    // Closing any descriptor of the file lets go of the process's locks on it
    p.sys(57, &[I(4)]).expect(0);
    let child = p.fork();
    p.sys(25, &[I(3), I(6), D(0x380)]).expect(0);
    p.parent(child);
    p.sys(260, &[I(0xffff_ffff), D(0x500), I(0), I(0)]);
    p.arg(0, W(0x500));
    p.expect(0);
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn flock_is_shared_or_exclusive_and_lock_nb_does_not_wait() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/flocked\0");
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0o102), I(0o644)]).expect(3);
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(2), I(0)]).expect(4);
    p.sys(32, &[I(3), I(1)]).expect(0); // LOCK_SH
    p.sys(32, &[I(4), I(1)]).expect(0); // another description shares it
    p.sys(32, &[I(4), I(2 | 4)]).expect(-11); // LOCK_EX | LOCK_NB: 3 shares it
    p.sys(32, &[I(3), I(8)]).expect(0); // LOCK_UN
    p.sys(32, &[I(4), I(2 | 4)]).expect(0);
    p.sys(32, &[I(3), I(1 | 4)]).expect(-11);
    p.sys(57, &[I(4)]).expect(0); // its description closed, its lock goes
    p.sys(32, &[I(3), I(2 | 4)]).expect(0);
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn a_device_opened_from_a_descriptor_of_dev_is_the_device() {
    // wasmtime's guests open every file from a preopened directory: the Erlang VM of supa-wasm's
    // Realtime opens `null` from one of /dev, and looped on ENOENT
    let mut p = Prog::default();
    p.put(0x200, b"/dev\0").put(0x210, b"null\0").put(0x220, b"zero\0");
    // openat(AT_FDCWD, "/dev", O_PATH | O_DIRECTORY | O_CLOEXEC)
    p.sys(56, &[I(0xffff_ff9c), D(0x200), I(0x284000), I(0)]).expect(3);
    // openat(3, "null", O_WRONLY | O_NOFOLLOW | O_CLOEXEC): /dev/null, which takes what it's given
    p.sys(56, &[I(3), D(0x210), I(0xa8001), I(0)]).expect(4);
    p.sys(64, &[I(4), D(0x210), I(4)]).expect(4);
    // and /dev/zero reads zeros; fstatat sees a character device
    p.put(0x300, &[7u8; 4]);
    p.sys(56, &[I(3), D(0x220), I(0), I(0)]).expect(5);
    p.sys(63, &[I(5), D(0x300), I(4)]).expect(4);
    p.is(0x300, 0);
    p.sys(79, &[I(3), D(0x210), D(0x400), I(0)]).expect(0);
    p.arg(0, W(0x400 + 16)); // st_mode
    p.code.push(0xd34c_fc00); // lsr x0, x0, #12: the file type
    p.expect(2); // S_IFCHR >> 12
    let (_, status) = run_linked(p);
    ok(&status);
}

#[test]
fn the_host_starts_a_process_with_pipes_and_takes_its_output_and_status() {
    // snowglobe's jobs on a site with no kernel: a shell command run beside the program
    let mut p = Prog::default();
    p.put(0x200, b"out\n").put(0x210, b"err\n");
    p.sys(173, &[]); // getppid: 0 for the one the host started
    let at = p.code.len();
    p.code.push(0); // cbnz x0: the first process's part
    p.out(0x200, 4);
    p.sys(64, &[I(2), D(0x210), I(4)]);
    p.sys(63, &[I(0), D(0x300), I(4)]).expect(4);
    p.sys(64, &[I(1), D(0x300), I(4)]);
    p.sys(63, &[I(0), D(0x300), I(4)]).expect(0); // its stdin closed: the end
    p.sys(94, &[I(3)]);
    p.code[at] = 0xb500_0000 | ((p.code.len() - at) as u32) << 5;
    // The first process waits on the terminal
    p.sys(63, &[I(0), D(0x300), I(1)]);
    let mut vm = machine_with_prog(&p.elf());
    let board = vm.board.clone();
    let mut proc = Process::exec(&mut vm.cpus[0], board, &["/usr/bin/prog".into()], &[], "/").unwrap();
    let mut step = |proc: &mut Process| proc.run_on(&mut vm.cpus, std::time::Duration::from_millis(20));
    step(&mut proc);
    let pid = proc.spawn(&["prog".into()], &["PATH=/usr/bin".into()], "/tmp", None).unwrap();
    assert_ne!(pid, 1000);
    assert!(proc.spawn_input(pid, b"ping"));
    proc.spawn_close_input(pid);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let begun = std::time::Instant::now();
    let exit = loop {
        step(&mut proc);
        let r = proc.spawn_poll(pid).unwrap();
        out.extend(r.stdout);
        err.extend(r.stderr);
        if let Some(e) = r.exit {
            break e;
        }
        assert!(begun.elapsed() < std::time::Duration::from_secs(10), "still running");
    };
    assert_eq!((out.as_slice(), err.as_slice(), exit), (&b"out\nping"[..], &b"err\n"[..], 3));
    // Released: gone, and the first process goes on waiting
    proc.spawn_release(pid);
    assert!(proc.spawned().is_empty());
    assert!(proc.spawn_poll(pid).is_none());
    assert_eq!(proc.processes(), 1);
    assert!(proc.waiting_for_input());
}

const ENOENT: i64 = -2;
/// O_RDONLY | O_DIRECTORY | O_CLOEXEC
const O_DIR: u64 = 0o2_200_000;

/// read(fd, data[at..], 256), then write(1) of what it read
fn cat(p: &mut Prog, fd: u64, at: u32) {
    p.sys(63, &[I(fd), D(at), I(256)]).save(0x180);
    p.sys(64, &[I(1), D(at), W(0x180)]);
}

/// The names and types of the linux_dirent64 records in `b`, and the position each says is next
fn dirents(mut b: &[u8]) -> Vec<(String, u8, u64)> {
    let mut out = Vec::new();
    while b.len() >= 19 {
        let off = u64::from_le_bytes(b[8..16].try_into().unwrap());
        let reclen = u16::from_le_bytes([b[16], b[17]]) as usize;
        let name = b[19..reclen].split(|&c| c == 0).next().unwrap();
        out.push((String::from_utf8_lossy(name).into_owned(), b[18], off));
        b = &b[reclen..];
    }
    out
}

#[test]
fn proc_lists_every_process_with_its_command_line_and_state() {
    // busybox's ps and pgrep, and snowglobe's `for f in /proc/[0-9]*/cmdline`: the first
    // process (1000, run as `/usr/bin/prog sleep 4167`) forks 1001, which waits on a pipe
    let mut p = Prog::default();
    p.put(0x200, b"/proc/\0").put(0x210, b"/proc/1001/cmdline\0");
    p.put(0x230, b"/proc/1001/stat\0").put(0x240, b"/proc/self/stat\0");
    p.put(0x250, b"/proc/1001/status\0").put(0x270, b"/proc/1001\0");
    p.put(0x280, b"/proc/1002\0").put(0x290, b"\n--\n");
    // struct pollfd for descriptor 10, POLLIN
    p.put(0x1a0, &10u32.to_le_bytes()).put(0x1a4, &1u16.to_le_bytes());
    p.sys(59, &[D(0x100), I(0)]).expect(0); // a pipe: 3, 4
    let child = p.fork();
    p.sys(63, &[I(3), D(0x300), I(1)]); // waits for the parent
    p.parent(child);
    p.expect(1001);
    // The image has no /proc: opening it (as musl's glob does, with its slash) still lists
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(O_DIR), I(0)]).expect(5);
    p.sys(61, &[I(5), D(0x800), I(8)]).expect(EINVAL); // no room for one
    // One entry a call (each is 24 bytes), to the end
    for _ in 0..6 {
        p.sys(61, &[I(5), D(0x800), I(24)]).save(0x180);
        p.sys(64, &[I(1), D(0x800), W(0x180)]);
    }
    p.out(0x290, 4);
    p.sys(56, &[I(AT_FDCWD), D(0x210), I(0), I(0)]).expect(6);
    cat(&mut p, 6, 0x900);
    p.out(0x290, 4);
    p.sys(56, &[I(AT_FDCWD), D(0x230), I(0), I(0)]).expect(7);
    cat(&mut p, 7, 0x900);
    p.out(0x290, 4);
    p.sys(56, &[I(AT_FDCWD), D(0x240), I(0), I(0)]).expect(8);
    cat(&mut p, 8, 0x900);
    p.out(0x290, 4);
    p.sys(56, &[I(AT_FDCWD), D(0x250), I(0), I(0)]).expect(9);
    cat(&mut p, 9, 0x900);
    // /proc/1001 is a directory; there is no 1002
    p.sys(79, &[I(AT_FDCWD), D(0x270), D(0xa00), I(0)]).expect(0);
    p.arg(0, W(0xa00 + 16)); // st_mode
    p.code.push(0xd34c_fc00); // lsr x0, x0, #12: the file type
    p.expect(4); // S_IFDIR >> 12
    p.sys(79, &[I(AT_FDCWD), D(0x280), D(0xa00), I(0)]).expect(ENOENT);
    // The child goes on and ends; nobody has waited for it yet
    p.sys(64, &[I(4), D(0x290), I(1)]).expect(1);
    p.sys(434, &[I(1001), I(0)]).expect(10); // pidfd_open
    p.sys(73, &[D(0x1a0), I(1), I(0), I(0), I(8)]).expect(1);
    p.out(0x290, 4);
    p.sys(56, &[I(AT_FDCWD), D(0x230), I(0), I(0)]).expect(11);
    cat(&mut p, 11, 0x900);
    // A zombie's command line is empty, and its directory is there until it is waited for
    p.sys(56, &[I(AT_FDCWD), D(0x210), I(0), I(0)]).expect(12);
    p.sys(63, &[I(12), D(0x900), I(256)]).expect(0);
    p.sys(79, &[I(AT_FDCWD), D(0x270), D(0xa00), I(0)]).expect(0);
    p.sys(260, &[I(1001), D(0x380), I(0), I(0)]).expect(1001); // wait4
    p.sys(79, &[I(AT_FDCWD), D(0x270), D(0xa00), I(0)]).expect(ENOENT);
    p.sys(56, &[I(AT_FDCWD), D(0x210), I(0), I(0)]).expect(ENOENT);
    let mut vm = machine_with_prog(&p.elf());
    let board = vm.board.clone();
    let args = ["/usr/bin/prog".into(), "sleep".into(), "4167".into()];
    let mut proc = Process::exec(&mut vm.cpus[0], board, &args, &[], "/").unwrap();
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    // (The terminal wrote each \n as \r\n: back, for the records' bytes)
    let mut text = Vec::new();
    for (i, &c) in out.iter().enumerate() {
        if !(c == b'\r' && out.get(i + 1) == Some(&b'\n')) {
            text.push(c);
        }
    }
    let mut parts = Vec::new();
    let mut rest = &text[..];
    while let Some(i) = rest.windows(4).position(|w| w == b"\n--\n") {
        parts.push(&rest[..i]);
        rest = &rest[i + 4..];
    }
    parts.push(rest);
    assert_eq!(parts.len(), 6, "{:?}", String::from_utf8_lossy(&text));
    // ".", "..", self, then a directory for each process, each with where the next starts
    let listed = dirents(parts[0]);
    let names: Vec<&str> = listed.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, [".", "..", "self", "1000", "1001"]);
    assert_eq!((listed[4].1, listed[2].1), (4, 10), "DT_DIR, DT_LNK");
    assert!(listed.windows(2).all(|w| w[0].2 < w[1].2), "{listed:?}");
    assert_eq!(parts[1], b"/usr/bin/prog\0sleep\x004167\0");
    let stat = String::from_utf8_lossy(parts[2]);
    // Waiting on its pipe (or, on another CPU, not there yet)
    assert!(stat.starts_with("1001 (prog) S 1000 1000 ") || stat.starts_with("1001 (prog) R 1000 1000 "), "{stat}");
    assert_eq!(stat.split_whitespace().count(), 52, "{stat}");
    assert_eq!(stat.split_whitespace().nth(19), Some("1"), "num_threads");
    assert!(String::from_utf8_lossy(parts[3]).starts_with("1000 (prog) R 1 1000 "));
    let status = String::from_utf8_lossy(parts[4]);
    for line in ["Name:\tprog\n", "Pid:\t1001\n", "Tgid:\t1001\n", "PPid:\t1000\n", "Uid:\t0\t0\t0\t0\n", "Threads:\t1\n"] {
        assert!(status.contains(line), "{line:?} in {status}");
    }
    assert!(String::from_utf8_lossy(parts[5]).starts_with("1001 (prog) Z 1000 "));
}

#[test]
fn a_restored_process_keeps_its_command_line() {
    // Saved while it waits for a key; restored, it reads its /proc/self/cmdline
    let mut p = Prog::default();
    p.put(0x200, b"/proc/self/cmdline\0");
    p.sys(63, &[I(0), D(0x380), I(1)]).expect(1);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).expect(3);
    cat(&mut p, 3, 0x900);
    let elf = p.elf();
    let mut vm = machine_with_prog(&elf);
    let board = vm.board.clone();
    let args = ["/usr/bin/prog".into(), "-c".into(), "a b".into()];
    let mut proc = Process::exec(&mut vm.cpus[0], board, &args, &[], "/").unwrap();
    let status = loop {
        match proc.run_on(&mut vm.cpus, std::time::Duration::from_millis(20)) {
            armless_user::Status::Running => {},
            s => break s,
        }
    };
    assert!(matches!(status, armless_user::Status::Idle(None)), "{status:?}");
    let process = proc.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((proc, vm));

    let mut vm = machine_with_prog(&elf);
    vm.load_state(&state).unwrap();
    let mut proc = Process::restore(&mut vm.cpus[0], Some(vm.board.clone()), &process).unwrap();
    proc.input(b"\n");
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    assert!(out.ends_with(b"/usr/bin/prog\0-c\0a b\0"), "{:?}", String::from_utf8_lossy(&out));
}

const SIGPIPE: u64 = 13;
const EPIPE: i64 = -32;
const MSG_NOSIGNAL: u64 = 0x4000;

/// Runs `p` on a machine of its own: how it ended
fn ends(p: Prog) -> armless_user::Status {
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    run(&mut proc, &mut vm).1
}

#[test]
fn a_write_to_a_pipe_nobody_reads_ends_the_program_by_sigpipe_unless_it_ignores_it() {
    // pipe2: 3 and 4; the read end closed, a write ends the program (as a shell says it, 141)
    let mut p = Prog::default();
    p.sys(59, &[D(0x100), I(0)]).expect(0);
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(4), D(0x200), I(1)]);
    let status = ends(p);
    assert!(matches!(status, armless_user::Status::Exited(141)), "{status:?}");

    // Ignored: the write is EPIPE
    let mut p = Prog::default();
    p.ignore_signal(SIGPIPE);
    p.sys(59, &[D(0x100), I(0)]).expect(0);
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(4), D(0x200), I(1)]).expect(EPIPE);
    ok(&ends(p));

    // Blocked: EPIPE, and the signal pending for the thread
    let mut p = Prog::default();
    p.put(0x180, &(1u64 << (SIGPIPE - 1)).to_le_bytes());
    p.sys(135, &[I(SIG_BLOCK), D(0x180), I(0), I(8)]).expect(0);
    p.sys(59, &[D(0x100), I(0)]).expect(0);
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(4), D(0x200), I(1)]).expect(EPIPE);
    p.sys(136, &[D(0x190), I(8)]).expect(0); // rt_sigpending
    p.arg(0, W(0x190));
    p.expect_wide(1 << (SIGPIPE - 1));
    ok(&ends(p));
}

#[test]
fn a_send_to_a_closed_stream_raises_sigpipe_unless_msg_nosignal() {
    // socketpair(AF_UNIX, SOCK_STREAM): 3 and 4; 4 closed
    let mut p = Prog::default();
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0);
    p.sys(57, &[I(4)]).expect(0);
    // sendto with MSG_NOSIGNAL: EPIPE and nothing else; then a write: the signal
    p.sys(206, &[I(3), D(0x200), I(1), I(MSG_NOSIGNAL), I(0), I(0)]).expect(EPIPE);
    p.sys(64, &[I(3), D(0x200), I(1)]);
    let status = ends(p);
    assert!(matches!(status, armless_user::Status::Exited(141)), "{status:?}");

    let mut p = Prog::default();
    p.ignore_signal(SIGPIPE);
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0);
    p.sys(57, &[I(4)]).expect(0);
    p.sys(206, &[I(3), D(0x200), I(1), I(0), I(0), I(0)]).expect(EPIPE);
    ok(&ends(p));
}
