//! Randomness in user mode: every copy of a saved process draws its own. getrandom and
//! /dev/urandom read the host's generator on each call, so nothing a snapshot holds is drawn
//! again by the copies made of it.

mod prog;

use std::time::{Duration, Instant};

use armless_user::{Process, Status};
use prog::*;

/// Runs until the program waits for input
fn until_idle(p: &mut Process, vm: &mut armless::vm::Vm<armless::platform::NativePlatform>) {
    let begun = Instant::now();
    while !matches!(p.run_on(&mut vm.cpus, Duration::from_millis(20)), Status::Idle(_)) {
        assert!(begun.elapsed() < Duration::from_secs(10), "never waited");
    }
}

/// Gives the waiting program its line, and what it prints then
fn draw(mut p: Process, mut vm: armless::vm::Vm<armless::platform::NativePlatform>) -> Vec<u8> {
    p.input(b"\n");
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert!(out.len() >= 32, "{out:?}");
    out
}

#[test]
fn every_restore_of_a_saved_process_draws_its_own_random_bytes() {
    let mut p = Prog::default();
    p.put(0x300, b"/dev/urandom\0");
    // Waits for a line, then draws 16 bytes with getrandom and 16 from /dev/urandom
    p.sys(63, &[I(0), D(0x100), I(1)]).expect(1);
    p.sys(278, &[D(0x200), I(16), I(0)]).expect(16);
    p.sys(56, &[I(0xffff_ff9c), D(0x300), I(0), I(0)]).expect(3);
    p.sys(63, &[I(3), D(0x210), I(16)]).expect(16);
    p.out(0x200, 32);

    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    until_idle(&mut proc, &mut vm);
    let saved = proc.save(&vm.cpus[0]);
    let state = vm.save_state();

    let mut draws = vec![draw(proc, vm)];
    for _ in 0..2 {
        let mut vm = machine();
        vm.load_state(&state).unwrap();
        let p = Process::restore(&mut vm.cpus[0], None, &saved, Duration::ZERO).unwrap();
        draws.push(draw(p, vm));
    }
    for (i, a) in draws.iter().enumerate() {
        for b in &draws[i + 1..] {
            assert_ne!(a, b, "two copies drew the same bytes");
        }
    }
}
