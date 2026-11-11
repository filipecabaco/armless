//! WebAssembly exports for user mode: a program with no guest kernel, on the machine of armless's
//! own exports (`armless::wasm`). armless.wasm is built from this crate, so the one module has
//! both: `armless_create` makes the machine, `armless_fs_load` gives it its filesystem (or
//! `armless_state_*` restores a snapshot of it), and these run a program on its CPU.
//!
//! The program's terminal is the machine's console: what it writes comes out of `armless_events`
//! as console output (type 2), and the files it reads that the filesystem doesn't hold yet as
//! blob events (type 3), which the host answers with `armless_blob*` as for a Linux guest. One
//! process, on CPU 0 (the single-CPU build).
//!
//! A snapshot is the pair a native host makes (`snowglobe-vm snapshot --user`): the machine's
//! state (`armless_save_*`, the same bytes as `Vm::save_state`) and the process's
//! (`armless_user_save`, those of `Process::save`), restored in that order.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use armless::wasm::{now_ms, out, set_error, slice, string, vm};
use armless::Event;

use crate::host::{self, Instant};
use crate::{ExecError, Process, Status};

std::thread_local! {
    /// The program, once `armless_user_exec` or `armless_user_restore` made it.
    static PROCESS: RefCell<Option<Process>> = const { RefCell::new(None) };
    /// When `armless_user_run` returned 1: when to run again, in `microtick()` ms, or -1.
    static WAKE_MS: Cell<f64> = const { Cell::new(-1.0) };
}

/// The process's randomness (getrandom, /dev/urandom, AT_RANDOM) from the host's: the machine's
/// last `armless_seed`, which the host gives it after every restore.
fn seed() {
    host::seed(&vm().board.boot_seed.lock());
}

/// A NUL-separated list ("a\0b\0"; empty items dropped).
fn list(s: &str) -> Vec<String> {
    s.split('\0')
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect()
}

/// Start a program from the machine's filesystem as execve would (by PATH from `env` when
/// `argv[0]` has no slash; a dynamically linked one with its loader): `argv` and `env` are
/// NUL-separated lists, `cwd` the working directory. Returns 0 when it is ready to run, 1 when
/// its files are still being fetched (blob events were raised: provide them, then call again),
/// -1 on failure (see `armless_error`). It replaces any process there was.
///
/// # Safety
/// Each pointer must reference its length of readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_user_exec(
    argv: u32,
    argv_len: u32,
    env: u32,
    env_len: u32,
    cwd: u32,
    cwd_len: u32,
) -> i32 {
    let (argv, env, cwd) = (
        list(&string(argv, argv_len)),
        list(&string(env, env_len)),
        string(cwd, cwd_len),
    );
    let cwd = if cwd.is_empty() { "/".to_string() } else { cwd };
    seed();
    let v = vm();
    match Process::exec(&mut v.cpus[0], v.board.clone(), &argv, &env, &cwd) {
        Ok(p) => {
            PROCESS.set(Some(p));
            0
        },
        Err(ExecError::Need(_)) => 1,
        Err(ExecError::Fail(e)) => {
            set_error(format!("{}: {e}", argv.first().map_or("", |a| a.as_str())));
            -1
        },
    }
}

/// Run the program for up to `budget_ms` milliseconds of wall time, or until all its threads
/// wait. What it wrote comes out as console events. Returns 0 when its time ran out (run it
/// again), 1 when it is idle (see `armless_user_wake_ms` and `armless_user_waiting_on_blobs`;
/// input, a resize or a blob that arrives wakes it), 2 when it has exited (see
/// `armless_user_exit_code`), 3 when it faulted (the reason in `armless_error`), -1 when there is
/// no process.
#[no_mangle]
pub extern "C" fn armless_user_run(budget_ms: f64) -> i32 {
    PROCESS.with_borrow_mut(|p| {
        let Some(p) = p.as_mut() else {
            set_error("no process");
            return -1;
        };
        let v = vm();
        let budget = Duration::from_secs_f64(budget_ms.max(0.0) / 1e3);
        let status = p.run(&mut v.cpus[0], budget);
        let output = p.take_output();
        if !output.is_empty() {
            v.board.io.out.push(Event::Console(output));
        }
        match status {
            Status::Running => 0,
            Status::Idle(wake) => {
                WAKE_MS.set(wake.map_or(-1.0, |w| {
                    now_ms() + w.saturating_duration_since(Instant::now()).as_secs_f64() * 1e3
                }));
                1
            },
            Status::Exited(_) => 2,
            Status::Fault(f) => {
                set_error(f);
                3
            },
        }
    })
}

/// After `armless_user_run` returned 1: the `microtick()` time at which to run it again (its
/// earliest deadline: a sleep, a timeout), or -1 when only input or blobs can wake it.
#[no_mangle]
pub extern "C" fn armless_user_wake_ms() -> f64 {
    WAKE_MS.get()
}

/// Whether a thread waits on contents of the filesystem the host hasn't given yet: when the
/// program is idle, this tells one busy reading files from one that waits for input.
#[no_mangle]
pub extern "C" fn armless_user_waiting_on_blobs() -> u32 {
    PROCESS.with_borrow(|p| p.as_ref().is_some_and(|p| p.waiting_on_blobs()) as u32)
}

/// Whether the program waits for input (`Process::waiting_for_input`): when it is idle, this
/// tells a REPL at its prompt from a program that only sleeps or waits on files.
#[no_mangle]
pub extern "C" fn armless_user_waiting_for_input() -> u32 {
    PROCESS.with_borrow(|p| p.as_ref().is_some_and(|p| p.waiting_for_input()) as u32)
}

/// The program's exit status (128 + the signal for one a signal ended), or -1 while it runs.
#[no_mangle]
pub extern "C" fn armless_user_exit_code() -> i32 {
    PROCESS.with_borrow(|p| p.as_ref().and_then(|p| p.exit_code()).unwrap_or(-1))
}

/// Bytes typed into the program's terminal (the line discipline echoes and edits them as its
/// termios says; Ctrl-C and the like are signals).
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_user_input(ptr: u32, len: u32) {
    let data = slice(ptr, len);
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.input(data)));
}

/// The end of the terminal's input: reads past what is left return 0.
#[no_mangle]
pub extern "C" fn armless_user_close_input() {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.close_input()));
}

/// The terminal's size (SIGWINCH for the program).
#[no_mangle]
pub extern "C" fn armless_user_resize(cols: u32, rows: u32) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.resize(cols as u16, rows as u16)));
}

/// A signal for the process (`Process::kill`), as `kill(2)` sends it: SIGINT (2) when a page's
/// stop button interrupts the program without going through the terminal.
#[no_mangle]
pub extern "C" fn armless_user_kill(sig: u32) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.kill(sig)));
}

/// The process's state for a snapshot (`Process::save`): its length (see `armless_buf`), 0 when
/// there is no process. The machine's own state (`armless_save_*`) holds its memory, CPU and
/// filesystem; take both with the CPU stopped.
#[no_mangle]
pub extern "C" fn armless_user_save() -> u32 {
    PROCESS.with_borrow_mut(|p| match p.as_mut() {
        Some(p) => out(p.save(&vm().cpus[0])),
        None => 0,
    })
}

/// The process from its saved state (`armless_user_save`, or `Process::save` natively), on the
/// machine restored from the snapshot it was saved with (`armless_state_*` first). It asks again
/// for blobs it was waiting on. Returns 0 or -1 (see `armless_error`).
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_user_restore(ptr: u32, len: u32) -> i32 {
    seed();
    let v = vm();
    match Process::restore(&mut v.cpus[0], Some(v.board.clone()), slice(ptr, len)) {
        Ok(p) => {
            PROCESS.set(Some(p));
            0
        },
        Err(e) => {
            set_error(e);
            -1
        },
    }
}
