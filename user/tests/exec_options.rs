//! What `ExecOptions` change: the standard streams of a program with no terminal (a container's),
//! and the hostname. Programs are machine code (see `prog`).

mod prog;

use std::time::Duration;

use armless::vm::Vm;
use armless_user::{ExecError, ExecOptions, Process, Status, StdStreams};
use prog::*;

const AT_FDCWD: u64 = 0xffff_ff9c;
const TCGETS: u64 = 0x5401;
const EBADF: i64 = -9;
const ENOTTY: i64 = -25;
const S_IFMT: u32 = 0o170000;
const S_IFCHR: u32 = 0o020000;

type Machine = Vm<armless::platform::NativePlatform>;

fn start_with(vm: &mut Machine, p: Prog, opts: &ExecOptions) -> Process {
    let board = Some(vm.board.clone());
    Process::exec_bytes_with(&mut vm.cpus[0], board, &p.elf(), &["p".into()], &[], opts).unwrap()
}

fn plain() -> ExecOptions {
    ExecOptions {
        std: StdStreams::Plain,
        ..ExecOptions::default()
    }
}

/// Runs `p` until it waits or ends
fn run_until_idle(p: &mut Process, vm: &mut Machine) -> (Vec<u8>, Status) {
    let mut out = Vec::new();
    loop {
        let status = p.run_on(&mut vm.cpus, Duration::from_millis(20));
        out.extend(p.take_output());
        if !matches!(status, Status::Running) {
            return (out, status);
        }
    }
}

#[test]
fn plain_standard_streams_are_no_terminal_and_stdin_is_dev_null() {
    let mut p = Prog::default();
    p.put(0x200, b"a\nb\n");
    p.put(0x210, b"e\n");
    p.put(0x220, b"c\n");
    p.put(0x230, b"/dev/stdout\0");
    p.put(0x240, b"/dev/stdin\0");
    // stdin reads end of file at once, and none of them is a terminal
    p.sys(63, &[I(0), D(0x300), I(16)]).expect(0);
    for fd in 0..3 {
        p.sys(29, &[I(fd), I(TCGETS), D(0x300)]).expect(ENOTTY);
    }
    // stdin is a character device like /dev/null, stdout one too
    p.sys(80, &[I(0), D(0x400)]).expect(0);
    p.out(0x400 + 16, 4);
    p.sys(80, &[I(1), D(0x400)]).expect(0);
    p.out(0x400 + 16, 4);
    // stdin is not open for writing
    p.sys(64, &[I(0), D(0x200), I(1)]).expect(EBADF);
    // Written as it is: no "\r\n", and stderr goes to the same output
    p.out(0x200, 4);
    p.sys(64, &[I(2), D(0x210), I(2)]).expect(2);
    // What /dev/stdout and /dev/stdin open is what descriptors 1 and 0 are
    p.sys(56, &[I(AT_FDCWD), D(0x230), I(1), I(0)]).save(0x100).expect_ok();
    p.sys(64, &[W(0x100), D(0x220), I(2)]).expect(2);
    p.sys(29, &[W(0x100), I(TCGETS), D(0x300)]).expect(ENOTTY);
    p.sys(56, &[I(AT_FDCWD), D(0x240), I(0), I(0)]).save(0x108).expect_ok();
    p.sys(63, &[W(0x108), D(0x300), I(16)]).expect(0);
    let mut vm = machine();
    let mut p = start_with(&mut vm, p, &plain());
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    let mode = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
    assert_eq!(mode(0) & S_IFMT, S_IFCHR);
    assert_eq!(mode(4) & S_IFMT, S_IFCHR);
    assert_eq!(&out[8..], b"a\nb\ne\nc\n");
}

#[test]
fn the_terminal_is_the_default() {
    let mut p = Prog::default();
    p.put(0x200, b"a\n");
    p.sys(29, &[I(0), I(TCGETS), D(0x300)]).expect(0);
    p.out(0x200, 2);
    let mut vm = machine();
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"a\r\n");
}

#[test]
fn the_hostname_is_what_uname_and_proc_say() {
    let program = || {
        let mut p = Prog::default();
        p.put(0x200, b"/proc/sys/kernel/hostname\0");
        p.sys(160, &[D(0x400)]).expect(0);
        // utsname's nodename: 65 bytes in
        p.out(0x400 + 65, 6);
        p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).save(0x100).expect_ok();
        p.sys(63, &[W(0x100), D(0x500), I(64)]).expect(7);
        p.out(0x500, 7);
        p
    };
    let mut vm = machine();
    let opts = ExecOptions {
        hostname: Some("pod-7a".into()),
        ..ExecOptions::default()
    };
    let mut p = start_with(&mut vm, program(), &opts);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"pod-7apod-7a\r\n");

    // "armless" without one
    let mut p = Prog::default();
    p.sys(160, &[D(0x400)]).expect(0);
    p.out(0x400 + 65, 7);
    let mut vm = machine();
    let mut p = start_with(&mut vm, p, &ExecOptions::default());
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"armless");

    // utsname's field holds 64 bytes and the NUL
    for name in ["x".repeat(65), "a\0b".to_string()] {
        let mut vm = machine();
        let board = Some(vm.board.clone());
        let opts = ExecOptions {
            hostname: Some(name),
            ..ExecOptions::default()
        };
        let r = Process::exec_bytes_with(&mut vm.cpus[0], board, &Prog::default().elf(), &["p".into()], &[], &opts);
        assert!(matches!(r, Err(ExecError::Fail(_))));
    }
}

#[test]
fn the_options_survive_a_save_and_restore() {
    let mut p = Prog::default();
    p.put(0x200, &[0u8; 8]);
    p.put(0x208, &200_000_000u64.to_le_bytes());
    p.put(0x300, b"x\n");
    // Waits (a timer), so it can be saved, then goes on in the restored machine
    p.sys(101, &[D(0x200), I(0)]).expect(0);
    p.sys(160, &[D(0x400)]).expect(0);
    p.out(0x400 + 65, 6);
    p.sys(29, &[I(1), I(TCGETS), D(0x500)]).expect(ENOTTY);
    // Still no terminal to open
    p.put(0x310, b"/dev/tty\0");
    p.sys(56, &[I(AT_FDCWD), D(0x310), I(2), I(0)]).expect(-6);
    p.out(0x300, 2);
    let mut vm = machine();
    let opts = ExecOptions {
        std: StdStreams::Plain,
        hostname: Some("pod-7a".into()),
    };
    let mut p = start_with(&mut vm, p, &opts);
    let (out, status) = run_until_idle(&mut p, &mut vm);
    assert!(out.is_empty(), "{status:?}");
    assert!(matches!(status, Status::Idle(Some(_))), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], None, &process, Duration::ZERO).unwrap();
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"pod-7ax\n");
}

#[test]
fn a_saved_state_with_a_hostname_that_does_not_fit_is_refused() {
    // A process that waits (a timer), saved with "pod-7a": the hostname is the state's last field
    let mut p = Prog::default();
    p.put(0x200, &[0u8; 8]);
    p.put(0x208, &200_000_000u64.to_le_bytes());
    p.sys(101, &[D(0x200), I(0)]).expect(0);
    p.sys(160, &[D(0x400)]).expect(0);
    p.out(0x400 + 65, 6);
    let mut vm = machine();
    let opts = ExecOptions {
        hostname: Some("pod-7a".into()),
        ..ExecOptions::default()
    };
    let mut p = start_with(&mut vm, p, &opts);
    let (_, status) = run_until_idle(&mut p, &mut vm);
    assert!(matches!(status, Status::Idle(Some(_))), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let with = |name: &[u8]| {
        let mut forged = process[..process.len() - (8 + "pod-7a".len())].to_vec();
        forged.extend_from_slice(&(name.len() as u64).to_le_bytes());
        forged.extend_from_slice(name);
        forged
    };
    let restore = |data: &[u8]| {
        let mut vm = machine();
        vm.load_state(&state).unwrap();
        Process::restore(&mut vm.cpus[0], None, data, Duration::ZERO).map(|_| ())
    };
    // (The forgery is a state that loads when the name is good)
    assert_eq!(restore(&with(b"pod-7b")), Ok(()));
    assert_eq!(restore(&with(&[b'x'; 64])), Ok(()));
    // More than utsname's field holds (uname would write past it), or a NUL in it
    assert!(restore(&with(&[b'x'; 65])).is_err());
    assert!(restore(&with(&[b'x'; 326])).is_err());
    assert!(restore(&with(b"a\0b")).is_err());
    // Not UTF-8
    assert!(restore(&with(b"\xff")).is_err());
    // Standard streams that are neither the terminal (0) nor plain (1): the field before the name
    let at = process.len() - (8 + "pod-7a".len()) - 8;
    for (std, good) in [(1u64, true), (2, false)] {
        let mut forged = process.clone();
        forged[at..at + 8].copy_from_slice(&std.to_le_bytes());
        assert_eq!(restore(&forged).is_ok(), good, "{std}");
    }
}

#[test]
fn a_state_saved_before_the_options_has_the_terminal_and_armless() {
    let mut p = Prog::default();
    p.put(0x200, &[0u8; 8]);
    p.put(0x208, &200_000_000u64.to_le_bytes());
    p.put(0x310, b"/dev/tty\0");
    p.sys(101, &[D(0x200), I(0)]).expect(0);
    p.sys(160, &[D(0x400)]).expect(0);
    p.out(0x400 + 65, 7);
    // A terminal to open
    p.sys(56, &[I(AT_FDCWD), D(0x310), I(2), I(0)]).expect_ok();
    let mut vm = machine();
    let opts = ExecOptions {
        std: StdStreams::Plain,
        hostname: Some("pod-7a".into()),
    };
    let mut p = start_with(&mut vm, p, &opts);
    let (_, status) = run_until_idle(&mut p, &mut vm);
    assert!(matches!(status, Status::Idle(Some(_))), "{status:?}");
    let mut process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));
    // ARMLUSRA: neither the standard streams nor the hostname at the end
    process.truncate(process.len() - (8 + 8 + "pod-7a".len()));
    process[..8].copy_from_slice(b"ARMLUSRA");
    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], None, &process, Duration::ZERO).unwrap();
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"armless");
}

#[test]
fn plain_stdout_reopened_for_reading_reads_nothing() {
    let mut p = Prog::default();
    p.put(0x200, b"/dev/stdout\0");
    // O_RDWR: it opens, but there is nothing to read from an output
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(2), I(0)])
        .save(0x100)
        .expect_ok();
    p.sys(63, &[W(0x100), D(0x300), I(16)]).expect(EBADF);
    let mut vm = machine();
    let mut p = start_with(&mut vm, p, &plain());
    ok(&run(&mut p, &mut vm).1);
}

#[test]
fn proc_self_fd_of_plain_stdout_links_to_dev_stdout() {
    let mut p = Prog::default();
    p.put(0x200, b"/proc/self/fd/1\0");
    p.sys(78, &[I(AT_FDCWD), D(0x200), D(0x300), I(64)])
        .expect(11);
    p.out(0x300, 11);
    let mut vm = machine();
    let mut p = start_with(&mut vm, p, &plain());
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"/dev/stdout");
}

#[test]
fn dev_stdin_on_a_proc_file_reads_it_from_the_start() {
    let mut p = Prog::default();
    p.put(0x200, b"/proc/sys/kernel/hostname\0");
    p.put(0x220, b"/dev/stdin\0");
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)])
        .save(0x100)
        .expect_ok();
    p.sys(63, &[W(0x100), D(0x300), I(2)]).expect(2);
    p.sys(24, &[W(0x100), I(0), I(0)]).expect(0);
    // A description of its own: not where descriptor 0 was in it
    p.sys(56, &[I(AT_FDCWD), D(0x220), I(0), I(0)])
        .save(0x108)
        .expect_ok();
    p.sys(63, &[W(0x108), D(0x400), I(64)]).expect(7);
    p.out(0x400, 7);
    let mut vm = machine();
    let opts = ExecOptions {
        hostname: Some("pod-7a".into()),
        ..ExecOptions::default()
    };
    let mut p = start_with(&mut vm, p, &opts);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"pod-7a\r\n");
}

#[test]
fn armless_exec_takes_plain_std_and_a_hostname() {
    let mut p = Prog::default();
    p.put(0x200, b"\n");
    p.sys(29, &[I(1), I(TCGETS), D(0x300)]).expect(ENOTTY);
    p.sys(160, &[D(0x400)]).expect(0);
    p.out(0x400 + 65, 6);
    p.out(0x200, 1);
    let elf = p.elf();
    // The program as a file of this host's, and in a filesystem (--fs) as /p
    let dir = std::env::temp_dir().join(format!("armless-exec-options-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("blobs")).unwrap();
    std::fs::write(dir.join("p"), &elf).unwrap();
    std::fs::write(dir.join("blobs/p"), &elf).unwrap();
    let json = format!(
        r#"{{"version":3,"fsroot":[["p",{},0,33261,0,0,"p"]]}}"#,
        elf.len()
    );
    std::fs::write(dir.join("fs.json"), json).unwrap();
    let exec = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_armless-exec"))
            .args(["-m", "128"])
            .args(args)
            .output()
            .unwrap()
    };
    let host = dir.join("p");
    let host = host.to_str().unwrap();
    let fs = dir.to_str().unwrap();
    let good = exec(&["--plain-std", "--hostname", "pod-7a", host]);
    let in_fs = exec(&["--fs", fs, "--plain-std", "--hostname", "pod-7a", "/p"]);
    let long = exec(&["--hostname", &"x".repeat(65), host]);
    let empty = exec(&["--plain-std", "--hostname", "", host]);
    std::fs::remove_dir_all(&dir).unwrap();
    for out in [&good, &in_fs] {
        assert_eq!(out.status.code(), Some(0));
        // No "\r\n": no terminal
        assert_eq!(out.stdout, b"pod-7a\n");
    }
    // An empty name is the default, as in the browser (its first 6 bytes)
    assert_eq!(empty.status.code(), Some(0));
    assert_eq!(empty.stdout, b"armles\n");
    assert_eq!(long.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&long.stderr).contains("not a hostname"));
}

#[test]
fn dev_std_follows_what_the_descriptors_were_redirected_to() {
    // stdin is a pipe: /dev/stdin is the same pipe
    let mut p = Prog::default();
    p.put(0x200, b"/dev/stdin\0");
    p.put(0x220, b"/dev/stdout\0");
    p.put(0x240, b"data");
    p.put(0x250, b"xy");
    p.sys(59, &[D(0x100), I(0)]).expect(0); // 3 reads, 4 writes
    p.sys(24, &[I(3), I(0), I(0)]).expect(0);
    p.sys(64, &[I(4), D(0x240), I(4)]).expect(4);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).save(0x108).expect_ok();
    p.sys(63, &[W(0x108), D(0x300), I(16)]).expect(4);
    p.out(0x300, 4);
    // and stdout one: /dev/stdout writes into it
    p.sys(24, &[I(4), I(1), I(0)]).expect(1);
    p.sys(56, &[I(AT_FDCWD), D(0x220), I(1), I(0)]).save(0x110).expect_ok();
    p.sys(64, &[W(0x110), D(0x250), I(2)]).expect(2);
    p.sys(63, &[I(3), D(0x320), I(16)]).expect(2);
    p.sys(64, &[I(2), D(0x320), I(2)]).expect(2);
    let mut vm = machine();
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"dataxy");
}

#[test]
fn dev_stdout_on_a_file_opens_it_again() {
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0");
    p.put(0x210, b"/dev/stdout\0");
    p.put(0x220, b"hello");
    // O_CREAT | O_RDWR
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0o102), I(0o644)]).save(0x100).expect_ok();
    p.sys(64, &[W(0x100), D(0x220), I(5)]).expect(5);
    p.sys(24, &[W(0x100), I(1), I(0)]).expect(1);
    // Its own offset: it reads the file from the start, not from where stdout is
    p.sys(56, &[I(AT_FDCWD), D(0x210), I(2), I(0)]).save(0x108).expect_ok();
    p.sys(63, &[W(0x108), D(0x300), I(16)]).expect(5);
    p.sys(64, &[I(2), D(0x300), I(5)]).expect(5);
    // A machine with a filesystem: just an empty /tmp
    let config = armless::vm::Config {
        memory_size: 128 << 20,
        cpus: test_cpus(),
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let mut vm: Machine = armless::runner::new_vm(config).unwrap().0;
    let json = br#"{"version":3,"fsroot":[["tmp",0,0,17407,0,0,[]]]}"#;
    vm.board.load_filesystem(json).unwrap();
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"hello");
}

#[test]
fn dev_std_of_a_closed_descriptor_is_no_file() {
    const ENOENT: i64 = -2;
    let mut p = Prog::default();
    p.put(0x200, b"/dev/stdin\0");
    p.put(0x210, b"/dev/stderr\0");
    p.sys(57, &[I(0)]).expect(0);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).expect(ENOENT);
    p.sys(57, &[I(2)]).expect(0);
    p.sys(56, &[I(AT_FDCWD), D(0x210), I(1), I(0)]).expect(ENOENT);
    // stdout is still the terminal, and /dev/stdout opens it
    p.put(0x220, b"/dev/stdout\0");
    p.sys(56, &[I(AT_FDCWD), D(0x220), I(1), I(0)]).expect_ok();
    let mut vm = machine();
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn dev_stdin_on_a_file_renamed_to_dev_stdin_does_not_loop() {
    const ENOENT: i64 = -2;
    let mut p = Prog::default();
    p.put(0x200, b"/tmp/f\0");
    p.put(0x210, b"/dev/stdin\0");
    // O_CREAT | O_RDWR, as stdin, then named /dev/stdin in the filesystem
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0o102), I(0o644)])
        .save(0x100)
        .expect_ok();
    p.sys(24, &[W(0x100), I(0), I(0)]).expect(0);
    p.sys(38, &[I(AT_FDCWD), D(0x200), I(AT_FDCWD), D(0x210)])
        .expect(0);
    // Its only name is the device's, which opens the descriptor again: no file to reopen
    p.sys(56, &[I(AT_FDCWD), D(0x210), I(0), I(0)])
        .expect(ENOENT);
    let config = armless::vm::Config {
        memory_size: 128 << 20,
        cpus: test_cpus(),
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let mut vm: Machine = armless::runner::new_vm(config).unwrap().0;
    let json = br#"{"version":3,"fsroot":[["dev",0,0,16877,0,0,[]],["tmp",0,0,17407,0,0,[]]]}"#;
    vm.board.load_filesystem(json).unwrap();
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn plain_stdout_is_not_dev_null_and_dev_stdout_stats_as_it() {
    let mut p = Prog::default();
    p.put(0x200, b"/dev/null\0");
    p.put(0x210, b"/dev/stdout\0");
    p.sys(80, &[I(1), D(0x400)]).expect(0);
    p.sys(79, &[I(AT_FDCWD), D(0x200), D(0x500), I(0)])
        .expect(0);
    p.sys(79, &[I(AT_FDCWD), D(0x210), D(0x600), I(0)])
        .expect(0);
    for at in [0x400, 0x500, 0x600] {
        p.out(at, 48);
    }
    let mut vm = machine();
    let mut p = start_with(&mut vm, p, &plain());
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    // st_dev, st_ino, st_mode and st_rdev of each
    let stat = |n: usize| {
        let s = &out[48 * n..48 * (n + 1)];
        let u64_at = |at: usize| u64::from_le_bytes(s[at..at + 8].try_into().unwrap());
        (
            u64_at(0),
            u64_at(8),
            u32::from_le_bytes(s[16..20].try_into().unwrap()),
            u64_at(32),
        )
    };
    let (stdout, null, dev_stdout) = (stat(0), stat(1), stat(2));
    assert_eq!(stdout.2 & S_IFMT, S_IFCHR);
    // GNU grep writes nothing to a stdout with /dev/null's st_dev and st_ino
    assert_ne!((stdout.0, stdout.1), (null.0, null.1));
    assert_ne!(stdout.3, null.3);
    assert_eq!(dev_stdout, stdout);
}

#[test]
fn without_a_terminal_dev_tty_is_no_device() {
    const ENXIO: i64 = -6;
    let program = |expect: i64| {
        let mut p = Prog::default();
        // The terminal by each of its names
        for (i, path) in [&b"/dev/tty\0"[..], b"/dev/console\0", b"/dev/pts/0\0"]
            .into_iter()
            .enumerate()
        {
            let at = 0x200 + 0x20 * i as u32;
            p.put(at, path);
            p.sys(56, &[I(AT_FDCWD), D(at), I(2), I(0)]);
            if expect < 0 {
                p.expect(expect);
            } else {
                p.expect_ok();
            }
        }
        p
    };
    let mut vm = machine();
    let mut p = start_with(&mut vm, program(ENXIO), &plain());
    ok(&run(&mut p, &mut vm).1);
    // With the terminal, it is the terminal
    let mut vm = machine();
    let mut p = start(&mut vm, program(0));
    ok(&run(&mut p, &mut vm).1);
}
