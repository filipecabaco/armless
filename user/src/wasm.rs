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
use crate::spawn::SpawnPoll;
use crate::{ExecError, ExecOptions, Process, Status, StdStreams};

std::thread_local! {
    /// The program, once `armless_user_exec` or `armless_user_restore` made it.
    static PROCESS: RefCell<Option<Process>> = const { RefCell::new(None) };
    /// When `armless_user_run` returned 1: when to run again, in `microtick()` ms, or -1.
    static WAKE_MS: Cell<f64> = const { Cell::new(-1.0) };
    /// `armless_user_plain_std`: the next exec has no terminal.
    static PLAIN_STD: Cell<bool> = const { Cell::new(false) };
    /// `armless_user_hostname`: the bytes of the next exec's hostname.
    static HOSTNAME: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// What the last `armless_user_spawn_poll` took, for the calls that return it.
    static POLLED: RefCell<Option<SpawnPoll>> = const { RefCell::new(None) };
    /// What `armless_user_asleep_ms` set for the next `armless_user_restore`.
    static ASLEEP: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// The process's randomness (getrandom, /dev/urandom, AT_RANDOM) from the host's: the machine's
/// last `armless_seed`, which the host gives it after every restore.
fn seed() {
    host::seed(&vm().board.boot_seed.lock());
}

/// A NUL-separated list ("a\0b\0": each item ends with its NUL, so an empty one is kept, as
/// execve keeps an empty argument; the last NUL may be left out, and "" is no items).
fn list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let items = s.strip_suffix('\0').unwrap_or(s);
    items.split('\0').map(str::to_string).collect()
}

/// Start a program from the machine's filesystem as execve would (by PATH from `env` when
/// `argv[0]` has no slash; a dynamically linked one with its loader): `argv` and `env` are
/// NUL-separated lists, `cwd` the working directory. Returns 0 when it is ready to run, 1 when
/// its files are still being fetched (blob events were raised: provide them, then call again),
/// -1 on failure (see `armless_error`). It replaces any process there was. It starts with the
/// options set by `armless_user_plain_std` and `armless_user_hostname` and, once it returns 0 or
/// -1, goes back to the defaults (a 1 keeps them for the call that follows the blobs).
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
    // SAFETY: the caller's promise
    let (argv, env, cwd) = unsafe {
        (
            string(argv, argv_len),
            string(env, env_len),
            string(cwd, cwd_len),
        )
    };
    exec(list(&argv), list(&env), cwd)
}

/// The options the host set for this exec (`armless_user_plain_std`, `armless_user_hostname`).
fn pending_options() -> Result<ExecOptions, String> {
    // (Refused, not made UTF-8 by replacing bytes: that would be a name the host did not give)
    let hostname = HOSTNAME
        .with_borrow(|h| String::from_utf8(h.clone()))
        .map_err(|_| "hostname: not UTF-8")?;
    Ok(ExecOptions {
        std: if PLAIN_STD.get() { StdStreams::Plain } else { StdStreams::Terminal },
        hostname: (!hostname.is_empty()).then_some(hostname),
    })
}

/// The next exec's standard streams: nonzero gives the program those of a container without a
/// terminal (`StdStreams::Plain`: stdin reads end of file, stdout and stderr are no terminal),
/// zero the terminal (the default). Call it before `armless_user_exec`; that exec (once it is not
/// asking for blobs any more) puts it back to the default.
#[no_mangle]
pub extern "C" fn armless_user_plain_std(on: u32) {
    PLAIN_STD.set(on != 0);
}

/// The next exec's hostname, which is what uname says (empty: "armless"): makes room for `len`
/// bytes (at most 64, the longest name; a longer one fails with 0 and the reason in
/// `armless_error`) and returns where they are in the module's memory. Write the UTF-8 bytes
/// there, then call `armless_user_exec`, which reads them and puts the hostname back to the
/// default. The pointer is only good until the next call of this function or the exec, and the
/// memory may have grown since the module was last asked, so re-read it before writing.
#[no_mangle]
pub extern "C" fn armless_user_hostname(len: u32) -> u32 {
    if len > 64 {
        HOSTNAME.with_borrow_mut(Vec::clear);
        set_error(format!("hostname: {len} bytes is longer than 64"));
        return 0;
    }
    HOSTNAME.with_borrow_mut(|h| {
        h.clear();
        h.resize(len as usize, 0);
        h.as_mut_ptr() as u32
    })
}

fn exec(argv: Vec<String>, env: Vec<String>, cwd: String) -> i32 {
    let cwd = if cwd.is_empty() { "/".to_string() } else { cwd };
    seed();
    let v = vm();
    let r = pending_options().map_err(ExecError::Fail).and_then(|opts| {
        Process::exec_with(&mut v.cpus[0], v.board.clone(), &argv, &env, &cwd, &opts)
    });
    if !matches!(r, Err(ExecError::Need(_))) {
        PLAIN_STD.set(false);
        HOSTNAME.with_borrow_mut(Vec::clear);
    }
    match r {
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
    // (Not while a spawned process is at work: the machine isn't quiet)
    PROCESS.with_borrow(|p| {
        p.as_ref()
            .is_some_and(|p| p.waiting_for_input() && !p.spawn_busy()) as u32
    })
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

/// Whether the first process reaps its orphans (`Process::reap_orphans`: a program that is pid 1 of
/// a container). Call it after `armless_user_exec` and before the first `armless_user_run`; a
/// restored process keeps what it was saved with.
#[no_mangle]
pub extern "C" fn armless_user_reap_orphans(on: u32) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.reap_orphans = on != 0));
}

/// A signal for the process (`Process::kill`), as `kill(2)` sends it: SIGINT (2) when a page's
/// stop button interrupts the program without going through the terminal.
#[no_mangle]
pub extern "C" fn armless_user_kill(sig: u32) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.kill(sig)));
}

/// The process's state for a snapshot (`Process::save`): its length (see `armless_buf`), 0 when
/// it can't be taken (the reason in `armless_error`): there is no process, or a process started
/// with `armless_user_spawn` is still held (not released), whose pipes' host ends a snapshot
/// doesn't keep. The machine's own state (`armless_save_*`) holds its memory, CPU and filesystem;
/// take both with the CPU stopped.
#[no_mangle]
pub extern "C" fn armless_user_save() -> u32 {
    PROCESS.with_borrow_mut(|p| match p.as_mut() {
        Some(p) if !p.spawned().is_empty() => {
            set_error(format!(
                "{} spawned process(es) not released: a snapshot doesn't keep the host's ends of their pipes",
                p.spawned().len()
            ));
            0
        },
        Some(p) => out(p.save(&vm().cpus[0])),
        None => {
            set_error("no process");
            0
        },
    })
}

// ---------------------------------------------------------------- processes beside the program

/// Start `argv[0]` as a process of its own beside the program (`Process::spawn`): its own session,
/// no parent, as root, with pipes for stdin, stdout and stderr whose other ends are the host's
/// (`armless_user_spawn_input` and `_poll`). `argv` and `env` are NUL-separated lists, `cwd` the
/// working directory. It runs while the host runs the machine (`armless_user_run`). Returns the pid,
/// -2 when its program's files are still being fetched (blob events were raised: provide them,
/// then call again), -1 on failure (see `armless_error`; also when there is no program).
///
/// # Safety
/// Each pointer must reference its length of readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_user_spawn(
    argv: u32,
    argv_len: u32,
    env: u32,
    env_len: u32,
    cwd: u32,
    cwd_len: u32,
) -> i64 {
    // SAFETY: the caller's promise
    let (argv, env, cwd) = unsafe {
        (
            string(argv, argv_len),
            string(env, env_len),
            string(cwd, cwd_len),
        )
    };
    let (argv, env) = (list(&argv), list(&env));
    let cwd = if cwd.is_empty() { "/".to_string() } else { cwd };
    seed();
    PROCESS.with_borrow_mut(|p| {
        let Some(p) = p.as_mut() else {
            set_error("no process");
            return -1;
        };
        match p.spawn(&argv, &env, &cwd, None) {
            Ok(pid) => pid,
            Err(ExecError::Need(_)) => -2,
            Err(ExecError::Fail(e)) => {
                set_error(e);
                -1
            },
        }
    })
}

/// Bytes for the stdin of the spawned process `pid`: 1 if accepted, 0 if it has no stdin any more
/// (closed, or not a process that is held).
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_user_spawn_input(pid: i64, ptr: u32, len: u32) -> i32 {
    let data = slice(ptr, len);
    PROCESS.with_borrow_mut(|p| p.as_mut().is_some_and(|p| p.spawn_input(pid, data)) as i32)
}

/// The end of the stdin of the spawned process `pid`: its reads past what is left return 0.
#[no_mangle]
pub extern "C" fn armless_user_spawn_close_input(pid: i64) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.spawn_close_input(pid)));
}

/// Takes what the spawned process `pid` did since the last poll: -1 for a pid that isn't a held
/// spawned process (unknown, or released), 0 otherwise. Read it with the calls that follow, which
/// answer for this poll until the next one: `armless_user_spawn_stdout` and
/// `armless_user_spawn_stderr` (what it printed: each returns its length, the bytes at
/// `armless_buf` until the next call that fills it), and `armless_user_spawn_exit`.
#[no_mangle]
pub extern "C" fn armless_user_spawn_poll(pid: i64) -> i32 {
    let polled = PROCESS.with_borrow_mut(|p| p.as_mut().and_then(|p| p.spawn_poll(pid)));
    let found = polled.is_some();
    POLLED.set(polled);
    if found { 0 } else { -1 }
}

/// What the last `armless_user_spawn_poll` took of stdout: its length (see `armless_buf`).
#[no_mangle]
pub extern "C" fn armless_user_spawn_stdout() -> u32 {
    out(POLLED.with_borrow_mut(|p| p.as_mut().map(|p| std::mem::take(&mut p.stdout)).unwrap_or_default()))
}

/// What the last `armless_user_spawn_poll` took of stderr: its length (see `armless_buf`).
#[no_mangle]
pub extern "C" fn armless_user_spawn_stderr() -> u32 {
    out(POLLED.with_borrow_mut(|p| p.as_mut().map(|p| std::mem::take(&mut p.stderr)).unwrap_or_default()))
}

/// The exit status the last `armless_user_spawn_poll` saw: -1 while it runs, else 0 to 255 (128
/// plus the signal for one a signal ended).
#[no_mangle]
pub extern "C" fn armless_user_spawn_exit() -> i32 {
    POLLED.with_borrow(|p| p.as_ref().and_then(|p| p.exit).unwrap_or(-1))
}

/// A signal for the process group of the spawned process `pid`: it and what it started.
#[no_mangle]
pub extern "C" fn armless_user_spawn_kill(pid: i64, sig: u32) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.spawn_kill(pid, sig)));
}

/// Done with the spawned process `pid`: its pipes let go and, once it has ended, the process taken
/// away. It no longer polls (-1), and no longer holds a snapshot back.
#[no_mangle]
pub extern "C" fn armless_user_spawn_release(pid: i64) {
    PROCESS.with_borrow_mut(|p| p.as_mut().map(|p| p.spawn_release(pid)));
}

/// How long the state the next `armless_user_restore` restores was saved for, in ms: the process's
/// monotonic clocks go on by that much and what was due in it is due now (`Process::restore`). 0
/// (the default) resumes where the process stopped, as does a negative, infinite or NaN value; one
/// too long for a `Duration` is the longest (`Process::restore` caps it). It applies to one
/// restore only.
#[no_mangle]
pub extern "C" fn armless_user_asleep_ms(ms: f64) {
    let asleep = if ms.is_finite() && ms > 0.0 {
        Duration::try_from_secs_f64(ms / 1e3).unwrap_or(Duration::MAX)
    } else {
        Duration::ZERO
    };
    ASLEEP.set(asleep);
}

/// The process from its saved state (`armless_user_save`, or `Process::save` natively), on the
/// machine restored from the snapshot it was saved with (`armless_state_*` first). It asks again
/// for blobs it was waiting on. How long the state sat saved is what `armless_user_asleep_ms` set
/// before (once: 0 after). Returns 0 or -1 (see `armless_error`).
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_user_restore(ptr: u32, len: u32) -> i32 {
    seed();
    let v = vm();
    // SAFETY: the caller's promise
    let state = unsafe { slice(ptr, len) };
    match Process::restore(&mut v.cpus[0], Some(v.board.clone()), state, ASLEEP.take()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A static ELF whose one instruction is a branch to itself: enough to make a process
    fn elf() -> Vec<u8> {
        const BASE: u64 = 0x40_0000;
        let mut e = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0".to_vec();
        e.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        e.extend_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
        e.extend_from_slice(&1u32.to_le_bytes());
        e.extend_from_slice(&(BASE + 120).to_le_bytes()); // entry, after the headers
        e.extend_from_slice(&64u64.to_le_bytes());
        e.extend_from_slice(&0u64.to_le_bytes());
        e.extend_from_slice(&0u32.to_le_bytes());
        for v in [64u16, 56, 1, 64, 0, 0] {
            e.extend_from_slice(&v.to_le_bytes());
        }
        e.extend_from_slice(&1u32.to_le_bytes()); // PT_LOAD, R X
        e.extend_from_slice(&5u32.to_le_bytes());
        for v in [0, BASE, BASE, 124, 124, 0x1000] {
            e.extend_from_slice(&v.to_le_bytes());
        }
        e.extend_from_slice(&0x1400_0000u32.to_le_bytes()); // b .
        e
    }

    fn set_hostname(name: &[u8]) {
        assert_ne!(armless_user_hostname(name.len() as u32), 0);
        HOSTNAME.with_borrow_mut(|h| h.copy_from_slice(name));
    }

    #[test]
    fn the_options_are_the_terminal_and_armless_unless_set() {
        let opts = pending_options().unwrap();
        assert_eq!((opts.std, opts.hostname), (StdStreams::Terminal, None));
        armless_user_plain_std(1);
        set_hostname(b"pod-7a");
        let opts = pending_options().unwrap();
        assert_eq!(
            (opts.std, opts.hostname.as_deref()),
            (StdStreams::Plain, Some("pod-7a"))
        );
        // Empty is the default, not an empty name
        armless_user_plain_std(0);
        armless_user_hostname(0);
        let opts = pending_options().unwrap();
        assert_eq!((opts.std, opts.hostname), (StdStreams::Terminal, None));
    }

    #[test]
    fn a_hostname_longer_than_64_bytes_or_not_utf8_is_refused() {
        set_hostname(&[b'x'; 64]);
        assert_eq!(armless_user_hostname(65), 0);
        assert_ne!(armless::wasm::armless_error_len(), 0);
        // and the one set before is gone
        assert_eq!(HOSTNAME.with_borrow(Vec::len), 0);
        set_hostname(b"\xff");
        assert!(pending_options().is_err());
    }

    #[test]
    fn exec_takes_the_options_once_it_has_its_files_and_then_forgets_them() {
        // SAFETY: no MAC to read (0)
        assert_eq!(
            unsafe { armless::wasm::armless_create(64, 1, armless::wasm::DEV_FS, 0) },
            0
        );
        let json = br#"{"version":3,"fsroot":[["x",124,0,33261,0,0,"x"]]}"#;
        vm().board.load_filesystem(json).unwrap();
        let x = || exec(vec!["/x".into()], Vec::new(), String::new());
        armless_user_plain_std(1);
        set_hostname(b"pod-7a");
        // Its blob is still to come: the options wait for the exec that follows
        assert_eq!(x(), 1);
        assert!(PLAIN_STD.get());
        assert_eq!(HOSTNAME.with_borrow(Vec::clone), b"pod-7a");
        vm().board.provide_raw_blob("x", elf()).unwrap();
        assert_eq!(x(), 0);
        PROCESS.with_borrow(|p| {
            let p = p.as_ref().unwrap();
            assert_eq!((p.std, p.hostname.as_str()), (StdStreams::Plain, "pod-7a"));
        });
        assert!(!PLAIN_STD.get());
        assert!(HOSTNAME.with_borrow(Vec::is_empty));
        // A failed exec forgets them too
        armless_user_plain_std(1);
        set_hostname(b"\xff");
        assert_eq!(x(), -1);
        assert!(!PLAIN_STD.get());
        assert!(HOSTNAME.with_borrow(Vec::is_empty));
    }

    #[test]
    fn reap_orphans_sets_the_option_of_the_process_there_is() {
        // None yet: nothing to set
        armless_user_reap_orphans(1);
        let config = armless::vm::Config {
            memory_size: 64 << 20,
            cpus: 1,
            devices: Default::default(),
        };
        let mut vm = armless::runner::new_vm(config).unwrap().0;
        let p = Process::exec_bytes(&mut vm.cpus[0], None, &elf(), &["p".into()], &[]).unwrap();
        assert!(!p.reap_orphans);
        PROCESS.set(Some(p));
        armless_user_reap_orphans(1);
        assert!(PROCESS.with_borrow(|p| p.as_ref().unwrap().reap_orphans));
        armless_user_reap_orphans(0);
        assert!(PROCESS.with_borrow(|p| !p.as_ref().unwrap().reap_orphans));
    }

    #[test]
    fn the_time_asleep_is_a_duration_or_none() {
        let asleep = |ms: f64| {
            armless_user_asleep_ms(ms);
            ASLEEP.take()
        };
        assert_eq!(asleep(1500.0), Duration::from_millis(1500));
        // None, a negative, an infinite or a NaN time: none
        for ms in [0.0, -0.0, -1.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            assert_eq!(asleep(ms), Duration::ZERO, "{ms}");
        }
        // Too long for a Duration: the longest
        assert_eq!(asleep(f64::MAX), Duration::MAX);
    }

    #[test]
    fn a_list_keeps_its_empty_items() {
        let v = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(list(""), v(&[]));
        assert_eq!(list("\0"), v(&[""]));
        assert_eq!(
            list("app\0--label\0\0next\0"),
            v(&["app", "--label", "", "next"])
        );
        assert_eq!(list("app\0\0"), v(&["app", ""]));
        assert_eq!(list("\0app\0"), v(&["", "app"]));
        // The last NUL is the terminator of the last item, not an item of its own
        assert_eq!(list("A=1\0B=\0"), v(&["A=1", "B="]));
        assert_eq!(list("a\0b"), v(&["a", "b"]));
    }
}
