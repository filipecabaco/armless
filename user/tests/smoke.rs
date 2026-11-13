//! Every system call user mode answers, made with arguments no program should give: all zero,
//! all ones, and pointers to nowhere (0x1, 0xdead0000) with huge lengths. Whatever a call is
//! given, the host must not panic (release builds abort on one, taking every program with them),
//! and what the call returns must be a result a system call may give: success, or an errno.

mod prog;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use armless_user::Status;
use prog::*;

/// The numbers the dispatcher (user/src/sys/mod.rs) answers, each made with every argument set.
/// Not made at all: exit (93) and exit_group (94), which end the program before it can check
/// anything; clone (220) and clone3 (435), whose junk stacks and flags make threads and
/// processes that run wherever they land; and rt_sigreturn (139), which takes a signal frame
/// from the stack. Calls that would wait forever with these arguments (a read of the terminal
/// or a pipe, ppoll, pselect6, rt_sigsuspend, rt_sigtimedwait, wait4 for a child) are given a
/// short time: a program still waiting then has passed.
#[rustfmt::skip]
const CALLS: &[u64] = &[
    0, 1, 2, 3, 4, 17, 19, 20, 21, 22, 23, 24, 25, 29, 32, 34, 35, 36, 37, 38, 43, 44, 45, 46,
    47, 48, 49, 50, 52, 53, 54, 55, 56, 57, 59, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72,
    73, 74, 78, 79, 80, 81, 82, 83, 85, 86, 87, 88, 90, 95, 96, 98, 99, 101, 102, 103, 107, 108,
    109, 110, 111, 113, 114, 115, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 129, 130,
    131, 132, 133, 134, 135, 136, 137, 140, 141, 143, 144, 145, 146, 147, 148, 149, 150, 151,
    152, 153, 154, 155, 156, 157, 158, 159, 160, 163, 164, 165, 166, 167, 168, 169, 172, 173,
    174, 175, 176, 177, 178, 179, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205,
    206, 207, 208, 209, 210, 211, 212, 214, 215, 216, 221, 222, 223, 226, 227, 228, 229, 233,
    235, 236, 237, 242, 243, 260, 261, 267, 269, 274, 275, 276, 278, 281, 283, 286, 287, 291,
    292, 293, 424, 434, 436, 438, 439, 441,
    // Numbers it doesn't answer: ENOSYS
    500, 1 << 20, u64::MAX,
];

/// Pointers to nowhere and lengths past anything
const NOWHERE: u64 = 0xdead_0000;
const HUGE: u64 = 0x7fff_ffff_ffff_ffff;

/// The argument sets each call is made with, the descriptors of `open_files` opened first or
/// not: (what it is, its six arguments, with the files)
fn argument_sets() -> Vec<(String, [A; 6], bool)> {
    let mut sets = vec![
        ("zero", [I(0); 6]),
        ("ones", [I(u64::MAX); 6]),
        (
            "nowhere",
            [I(1), I(NOWHERE), I(HUGE), I(1), I(NOWHERE), I(HUGE)],
        ),
        (
            "stdout and nowhere",
            [I(1), I(NOWHERE), I(u64::MAX), I(NOWHERE), I(1), I(u64::MAX)],
        ),
        // A buffer that is there (zeros), with lengths past it
        (
            "a buffer, huge lengths",
            [I(1), D(0x100), I(u64::MAX), D(0x100), I(HUGE), D(0x100)],
        ),
        (
            "ones then a buffer",
            [I(u64::MAX), D(0x100), I(HUGE), I(u64::MAX), D(0x100), I(1)],
        ),
        (
            "a buffer first",
            [
                D(0x100),
                I(u64::MAX),
                D(0x100),
                I(NOWHERE),
                I(u64::MAX),
                I(0),
            ],
        ),
    ]
    .into_iter()
    .map(|(what, args)| (what.to_string(), args, false))
    .collect::<Vec<_>>();
    // Each kind of descriptor, with a buffer and huge lengths, and with nowhere
    for fd in FIRST_FILE..=LAST_FILE {
        let a = [I(fd), D(0x100), I(u64::MAX), D(0x100), I(HUGE), I(1)];
        sets.push((format!("fd {fd}, a buffer"), a, true));
        let a = [I(fd), I(NOWHERE), I(0x40), I(0), I(u64::MAX), I(0)];
        sets.push((format!("fd {fd}, nowhere"), a, true));
    }
    sets
}

const FIRST_FILE: u64 = 3;
const LAST_FILE: u64 = 14;

/// One of each kind of descriptor, 3 to 14: a pipe's ends, a unix stream pair, an eventfd, an
/// epoll instance, a timerfd, a /proc file, /dev/zero, a TCP and a UDP socket, a signalfd
fn open_files(p: &mut Prog) {
    const AT_FDCWD: u64 = 0xffff_ff9c;
    p.put(0x200, b"/proc/self/fdinfo/0\0")
        .put(0x220, b"/dev/zero\0");
    p.sys(59, &[D(0x80), I(0)]); // 3, 4
    p.sys(199, &[I(1), I(1), I(0), D(0x88)]); // 5, 6
    p.sys(19, &[I(1), I(0)]); // 7
    p.sys(20, &[I(0)]); // 8
    p.sys(85, &[I(1), I(0)]); // 9
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]); // 10
    p.sys(56, &[I(AT_FDCWD), D(0x220), I(2), I(0)]); // 11
    p.sys(198, &[I(2), I(1), I(0)]); // 12
    p.sys(198, &[I(2), I(2), I(0)]); // 13
    p.sys(74, &[I(u64::MAX), D(0x90), I(8), I(0)]); // 14
}

/// How a call's program ended
#[derive(Debug)]
enum End {
    /// Its result was one a system call may give
    Valid,
    /// It still waited (or ran) when its time was up
    Waited,
    /// A signal or a fault ended it: what the call did to it (a junk handler, an unmapped
    /// stack), not what the call returned
    Ended(String),
    /// What the call returned is no result
    Invalid,
}

fn run_one(nr: u64, args: &[A; 6], with_files: bool) -> End {
    let mut p = Prog::default();
    if with_files {
        open_files(&mut p);
    }
    p.sys(nr, args).expect_valid();
    let invalid = 100 + p.checks as i32;
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let begun = Instant::now();
    loop {
        let status = proc.run_on(&mut vm.cpus, Duration::from_millis(10));
        proc.take_output();
        match status {
            Status::Exited(0) => return End::Valid,
            Status::Exited(c) if c == invalid => return End::Invalid,
            Status::Exited(c) => return End::Ended(format!("exit {c}")),
            Status::Fault(f) => return End::Ended(f),
            // Waiting for what nothing here will give (input, a wake) or until a deadline
            Status::Idle(None) => return End::Waited,
            Status::Idle(Some(_)) if begun.elapsed() > Duration::from_millis(50) => {
                return End::Waited;
            },
            Status::Idle(_) | Status::Running => {},
        }
        if begun.elapsed() > Duration::from_secs(2) {
            return End::Waited;
        }
    }
}

#[test]
fn every_system_call_with_junk_arguments_returns_a_result_and_never_panics() {
    let (mut bad, mut ended, mut waited) = (Vec::new(), Vec::new(), 0);
    let sets = argument_sets();
    for &nr in CALLS {
        for (what, args, with_files) in &sets {
            match catch_unwind(AssertUnwindSafe(|| run_one(nr, args, *with_files))) {
                Ok(End::Valid) => {},
                Ok(End::Waited) => waited += 1,
                Ok(End::Ended(how)) => ended.push(format!("{nr} ({what}): {how}")),
                Ok(End::Invalid) => bad.push(format!("{nr} ({what}): not a result")),
                Err(e) => {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    bad.push(format!("{nr} ({what}): panicked: {msg}"));
                },
            }
        }
    }
    eprintln!(
        "{} calls made, {waited} still waiting when their time was up",
        CALLS.len() * sets.len()
    );
    if !ended.is_empty() {
        eprintln!("ended by what the call did:\n{}", ended.join("\n"));
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
