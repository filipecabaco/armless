//! `Process::reap_orphans`: the first process (a container's program is pid 1) does not wait for
//! the processes whose parent ended; they leave as they end. Programs are machine code (see `prog`).

mod prog;

use std::time::Duration;

use armless::vm::Vm;
use armless_user::{Process, Status};
use prog::*;

const ECHILD: i64 = -10;
const B_PID: i64 = 1002;
const ENOENT: i64 = -2;
const AT_FDCWD: u64 = 0xffff_ff9c;
const WNOHANG: u64 = 1;
const WEXITED: u64 = 4;
const SIGCHLD_BIT: u64 = 1 << 16;

/// How the first process looks for an orphan, beside the `wait4` it always makes
#[derive(Clone, Copy, PartialEq)]
enum Probe {
    Wait4,
    /// `waitid(P_ALL, WEXITED)` instead of `wait4(-1)`
    Waitid,
    /// SIGCHLD is blocked, and what is pending is looked at when the orphan has ended
    Sigchld,
}

type Machine = Vm<armless::platform::NativePlatform>;

/// The program (pid 1) forks A, which forks B and ends. B tells its pid over a pipe, waits for a
/// byte and ends with status 7. The program waits for A (its child), looks at what it can wait
/// for while B lives, lets B end (the pipe's end of file says it has), and then looks again. With
/// `pause` it sleeps once it knows B's pid, so the machine can be saved with B alive.
fn orphan(reap: bool, pause: bool, probe: Probe) -> Prog {
    let mut p = Prog::default();
    p.put(0x200, b"x");
    p.put(0x260, b"/proc/1002\0");
    p.put(0x240, &[0u8; 8]);
    p.put(0x248, &200_000_000u64.to_le_bytes());
    p.put(0x2a0, &SIGCHLD_BIT.to_le_bytes());
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3 (read) and 4 (write)
    p.sys(59, &[D(0x108), I(0)]).expect(0); // pipe2: 5 and 6
    let a = p.fork();
    let b = p.fork();
    // B
    p.sys(172, &[]).save(0x180); // getpid
    p.sys(64, &[I(6), D(0x180), I(4)]).expect(4);
    p.sys(63, &[I(3), D(0x300), I(1)]).expect(1);
    p.sys(94, &[I(7)]);
    p.parent(b);
    // A ends at once, leaving B to the first process
    p.parent(a);
    // The program: A is its child, and ended well
    p.sys(260, &[I(0xffff_ffff), D(0x280), I(0), I(0)]).expect_ok();
    p.arg(0, W(0x280));
    p.expect(0);
    if probe == Probe::Sigchld {
        // A's SIGCHLD is behind; what comes now is the orphan's
        p.sys(135, &[I(0), D(0x2a0), I(0), I(8)]).expect(0);
    }
    p.sys(63, &[I(5), D(0x190), I(4)]).expect(4); // B's pid: the program is 1000 and A 1001
    p.arg(0, W(0x190));
    p.expect(B_PID);
    if pause {
        p.sys(101, &[D(0x240), I(0)]).expect(0);
    }
    // B lives. A child of the program that has not ended is "nothing yet"; an orphan is not one.
    if probe == Probe::Waitid {
        p.sys(95, &[I(0), I(0), D(0x280), I(WEXITED | WNOHANG), I(0)])
    } else {
        p.sys(260, &[I(0xffff_ffff), D(0x280), I(WNOHANG), I(0)])
    }
    .expect(if reap { ECHILD } else { 0 });
    // B goes on and ends; its pipe's end of file says so once the program has let go of the
    // write end, as B and A did
    p.sys(57, &[I(6)]).expect(0);
    p.sys(64, &[I(4), D(0x200), I(1)]).expect(1);
    p.sys(63, &[I(5), D(0x300), I(4)]).expect(0);
    if reap {
        // Nobody keeps it: it is gone, not a zombie to wait for
        p.sys(79, &[I(AT_FDCWD), D(0x260), D(0x400), I(0)]).expect(ENOENT);
        if probe == Probe::Waitid {
            p.sys(95, &[I(0), I(0), D(0x280), I(WEXITED), I(0)]).expect(ECHILD);
        } else {
            p.sys(260, &[I(0xffff_ffff), D(0x280), I(0), I(0)]).expect(ECHILD);
        }
    } else if probe == Probe::Waitid {
        p.sys(79, &[I(AT_FDCWD), D(0x260), D(0x400), I(0)]).expect(0);
        p.sys(95, &[I(0), I(0), D(0x280), I(WEXITED), I(0)]).expect(0);
        p.arg(0, W(0x290)); // si_pid
        p.expect(B_PID);
        p.arg(0, W(0x298)); // si_status
        p.expect(7);
    } else {
        // Init's child, as ever: a zombie until it is waited for
        p.sys(79, &[I(AT_FDCWD), D(0x260), D(0x400), I(0)]).expect(0);
        p.sys(260, &[I(0xffff_ffff), D(0x280), I(0), I(0)]).expect_ok();
        p.arg(0, W(0x280));
        p.expect(7 << 8);
    }
    if probe == Probe::Sigchld {
        p.sys(136, &[D(0x2a8), I(8)]).expect(0);
        p.arg(0, X(0x2a8));
        p.expect_wide(if reap { 0 } else { SIGCHLD_BIT });
    }
    p
}

/// The program forks A, which forks B and waits for B's end (the pipe's end of file) without
/// waiting for it, and ends: B is a zombie when its parent ends. The program waits for A only.
fn zombie_grandchild(reap: bool) -> Prog {
    let mut p = Prog::default();
    p.put(0x260, b"/proc/1002\0");
    let a = p.fork();
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3 (read) and 4 (write)
    let b = p.fork();
    p.parent(b); // B ends at once
    p.sys(57, &[I(4)]).expect(0);
    p.sys(63, &[I(3), D(0x300), I(1)]).expect(0); // B has ended
    p.parent(a); // A ends, B's zombie its own to keep
    p.sys(260, &[I(1001), D(0x280), I(0), I(0)]).expect(1001);
    // B, the child of an ended parent, is gone at once, or init's zombie
    p.sys(79, &[I(AT_FDCWD), D(0x260), D(0x400), I(0)])
        .expect(if reap { ENOENT } else { 0 });
    p.sys(260, &[I(0xffff_ffff), D(0x280), I(WNOHANG), I(0)]);
    if reap {
        p.expect(ECHILD);
    } else {
        p.expect(B_PID);
    }
    p
}

fn start_orphan(vm: &mut Machine, p: Prog, reap: bool) -> Process {
    let mut p = start(vm, p);
    p.reap_orphans = reap;
    p
}

#[test]
fn an_orphan_is_waited_for_by_the_first_process_unless_it_reaps_them() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, orphan(false, false, Probe::Wait4), false);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn a_reaping_first_process_neither_sees_nor_keeps_its_orphans() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, orphan(true, false, Probe::Wait4), true);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn a_reaping_first_process_gets_no_sigchld_for_an_orphan() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, orphan(true, false, Probe::Sigchld), true);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn waitid_of_a_reaping_first_process_does_not_see_its_orphans_either() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, orphan(true, false, Probe::Waitid), true);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn waitid_reports_an_orphan_of_a_first_process_that_does_not_reap() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, orphan(false, false, Probe::Waitid), false);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn a_zombie_child_of_an_ending_parent_is_dropped_at_once() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, zombie_grandchild(true), true);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn a_zombie_child_of_an_ending_parent_is_inits_without_reaping() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, zombie_grandchild(false), false);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn reaping_orphans_survives_a_save_and_restore_with_the_orphan_alive() {
    let mut vm = machine_with_cpus(1);
    let mut p = start_orphan(&mut vm, orphan(true, true, Probe::Wait4), true);
    // Runs until the program sleeps with B waiting for its byte
    let status = loop {
        let status = p.run_on(&mut vm.cpus, Duration::from_millis(20));
        if !matches!(status, Status::Running) {
            break status;
        }
    };
    assert!(matches!(status, Status::Idle(Some(_))), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine_with_cpus(1);
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], None, &process).unwrap();
    assert!(p.reap_orphans);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

/// The program, SIGCHLD blocked, clones A with no exit signal (so A's end raises none); A forks B,
/// which ends, and A ends without waiting for it: B is a zombie when it goes to the first process.
/// Then what is pending, and what the program can wait for.
fn zombie_grandchild_told(reap: bool) -> Prog {
    const WALL: u64 = 0x4000_0000;
    let mut p = Prog::default();
    p.put(0x2a0, &SIGCHLD_BIT.to_le_bytes());
    p.sys(135, &[I(0), D(0x2a0), I(0), I(8)]).expect(0);
    // clone(0): A, whose end nobody is signalled of
    p.sys(220, &[I(0), I(0), I(0), I(0), I(0)]);
    p.code.push(0);
    let a = p.code.len() - 1;
    p.sys(59, &[D(0x100), I(0)]).expect(0); // pipe2: 3 (read) and 4 (write)
    let b = p.fork();
    p.parent(b); // B ends at once
    p.sys(57, &[I(4)]).expect(0);
    p.sys(63, &[I(3), D(0x300), I(1)]).expect(0); // B has ended
    p.parent(a);
    p.sys(260, &[I(1001), D(0x280), I(WALL), I(0)]).expect(1001);
    // Linux tells the new parent of a child that had ended already: SIGCHLD
    p.sys(136, &[D(0x2a8), I(8)]).expect(0);
    p.arg(0, X(0x2a8));
    p.expect_wide(if reap { 0 } else { SIGCHLD_BIT });
    p
}

#[test]
fn a_zombie_that_goes_to_the_first_process_raises_sigchld_there() {
    let mut vm = machine();
    let mut p = start_orphan(&mut vm, zombie_grandchild_told(false), false);
    ok(&run(&mut p, &mut vm).1);
    let mut vm = machine();
    let mut p = start_orphan(&mut vm, zombie_grandchild_told(true), true);
    ok(&run(&mut p, &mut vm).1);
}

#[test]
fn a_zombie_that_goes_to_a_first_process_ignoring_sigchld_is_dropped() {
    // As zombie_grandchild, the program ignoring SIGCHLD: A goes as it ends, and so does B, the
    // zombie it leaves (Linux's reparenting drops it then)
    let mut p = Prog::default();
    p.ignore_signal(17);
    p.put(0x260, b"/proc/1002\0");
    let a = p.fork();
    // A takes SIGCHLD as it comes (SIG_DFL), so that B is its zombie
    p.put(0x3fc0, &[0u8; 32]);
    p.sys(134, &[I(17), D(0x3fc0), I(0), I(8)]).expect(0);
    p.sys(59, &[D(0x100), I(0)]).expect(0);
    let b = p.fork();
    p.parent(b);
    p.sys(57, &[I(4)]).expect(0);
    p.sys(63, &[I(3), D(0x300), I(1)]).expect(0);
    p.parent(a);
    // (Waits until A has gone: no child to report)
    p.sys(260, &[I(1001), D(0x280), I(0), I(0)]).expect(ECHILD);
    p.sys(79, &[I(AT_FDCWD), D(0x260), D(0x400), I(0)])
        .expect(ENOENT);
    p.sys(260, &[I(0xffff_ffff), D(0x280), I(WNOHANG), I(0)])
        .expect(ECHILD);
    let mut vm = machine();
    let mut p = start_orphan(&mut vm, p, false);
    ok(&run(&mut p, &mut vm).1);
}

#[test]
fn a_reaping_first_process_neither_sees_nor_keeps_its_orphans_on_every_cpu() {
    // (machine(): ARMLESS_TEST_CPUS of them)
    let mut vm = machine();
    let mut p = start_orphan(&mut vm, orphan(true, false, Probe::Wait4), true);
    ok(&run(&mut p, &mut vm).1);
    let mut vm = machine();
    let mut p = start_orphan(&mut vm, zombie_grandchild(true), true);
    ok(&run(&mut p, &mut vm).1);
}
