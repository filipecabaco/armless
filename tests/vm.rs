//! The snowglobe flow, natively: boot a kernel whose root is the 9p
//! filesystem (zstd blobs), talk to the guest over the virtio console, save
//! a snapshot, restore it into a fresh machine and keep talking.
//!
//! Needs a kernel and a filesystem made by tests/fs/make-9pfs.sh:
//!   ARMLESS_9PFS=DIR cargo test --release --test vm
//! (DIR has vmlinuz, fs.json and blobs/). Skipped without it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use armless::dev::{DeviceConfig, Event};
use armless::native::{HostOptions, NativeHost};
use armless::runner::{self, Runner};
use armless::vm::Config;

const CMDLINE: &str = "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose init=/init console=ttyS0 quiet";

fn assets() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("ARMLESS_9PFS")?);
    dir.join("fs.json").exists().then_some(dir)
}

#[derive(Default)]
struct Screen {
    text: Mutex<String>,
    cv: Condvar,
}

impl Screen {
    fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut t = self.text.lock().unwrap();
        while !t.contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {needle:?}; console so far:\n{t}"
            );
            t = self.cv.wait_timeout(t, left).unwrap().0;
        }
        let at = t.find(needle).unwrap() + needle.len();
        let before = t[..at].to_string();
        t.replace_range(..at, "");
        before
    }
}

struct Machine {
    runner: Runner,
    screen: Arc<Screen>,
    _host: NativeHost,
}

fn machine(dir: &Path, cpus: usize, boot: bool) -> Machine {
    let config = Config {
        memory_size: 256 << 20,
        cpus,
        devices: DeviceConfig {
            console: true,
            fs_tag: Some("host9p".into()),
            net: true,
            ..Default::default()
        },
    };
    let (mut vm, clock) = runner::new_vm(config).unwrap();
    vm.board
        .load_filesystem(&std::fs::read(dir.join("fs.json")).unwrap())
        .unwrap();
    if boot {
        let kernel = std::fs::read(dir.join("vmlinuz")).unwrap();
        vm.boot_linux(&kernel, None, CMDLINE).unwrap();
    }
    let screen = Arc::new(Screen::default());
    let s = screen.clone();
    let board = vm.board.clone();
    let host = NativeHost::spawn(
        board,
        HostOptions {
            blobs: Some(dir.join("blobs")),
            network: false,
            ..Default::default()
        },
        Box::new(move |e| {
            if let Event::Console(b) = e {
                s.text.lock().unwrap().push_str(&String::from_utf8_lossy(b));
                s.cv.notify_all();
            }
        }),
    );
    Machine {
        runner: Runner::new(vm, clock, false),
        screen,
        _host: host,
    }
}

fn run(m: &Machine, cmd: &str, marker: &str) -> String {
    m.runner
        .board
        .console_input(format!("{cmd}; echo {marker}\n").as_bytes());
    m.screen
        .wait_for(&format!("{marker}\r\n"), Duration::from_secs(120))
}

fn snapshot_flow(cpus: usize) {
    let Some(dir) = assets() else {
        eprintln!("skipping: set ARMLESS_9PFS to a directory made by tests/fs/make-9pfs.sh");
        return;
    };
    let mut a = machine(&dir, cpus, true);
    a.runner.start();
    a.screen.wait_for("READY", Duration::from_secs(120));
    let out = run(&a, "cat /etc/motd; grep -c ^processor /proc/cpuinfo", "M1");
    assert!(out.contains("hello from 9p"), "{out}");
    assert!(
        out.contains(&format!("{cpus}\r\n")) || out.contains(&format!("{cpus}\n")),
        "{out}"
    );
    // A file only the guest has: must survive the snapshot.
    run(
        &a,
        "echo kept > /root/note; sync; echo 3 > /proc/sys/vm/drop_caches",
        "M2",
    );

    let state = a.runner.save_state();
    drop(a);

    let mut b = machine(&dir, cpus, false);
    b.runner.load_state(&state).unwrap();
    b.runner.start();
    let out = run(
        &b,
        "cat /root/note; md5sum /data/big.bin; ls /data/small | wc -l",
        "M3",
    );
    assert!(out.contains("kept"), "{out}");
    assert!(out.contains("2b1902602225a2f8cd62faa5381597b4"), "{out}");
    assert!(out.contains("300"), "{out}");
    // The console size reaches the guest.
    b.runner.board.console_resize(132, 43);
    let out = run(&b, "sleep 1; stty size", "M4");
    assert!(out.contains("43 132"), "{out}");
}

#[test]
fn snapshot_one_cpu() {
    snapshot_flow(1);
}

#[test]
fn snapshot_two_cpus() {
    snapshot_flow(2);
}

/// What `/dev/urandom` gives on each CPU (`taskset`), once a newline reaches the shell that waits
/// for one: `R<cpu>:<hex>` lines up to `marker`.
fn draws(m: &Machine, marker: &str) -> Vec<String> {
    m.runner.board.console_input(b"\n");
    let out = m
        .screen
        .wait_for(&format!("{marker}\r\n"), Duration::from_secs(120));
    let draws: Vec<String> = out
        .split(|c: char| c.is_whitespace())
        .filter(|w| w.starts_with('R') && w.contains(':'))
        .map(str::to_string)
        .collect();
    assert!(!draws.is_empty(), "no draws in {out:?}");
    draws
}

/// A machine restored from `state`, running
fn restored(dir: &Path, cpus: usize, state: &[u8]) -> Machine {
    let mut m = machine(dir, cpus, false);
    m.runner.load_state(state).unwrap();
    m.runner.start();
    m
}

/// Copies of one snapshot don't draw the same random bytes, not even the first ones they draw,
/// on any CPU: each restore gives the guest a new VM generation ID, and Linux reseeds at once.
/// Taken while a shell waits for a line, so what it reads comes right after the restore. Needs a
/// kernel with the vmgenid driver (snowglobe's has it): skipped on one without.
fn fresh_randomness(cpus: usize) {
    let Some(dir) = assets() else {
        eprintln!("skipping: set ARMLESS_9PFS to a directory made by tests/fs/make-9pfs.sh");
        return;
    };
    let mut a = machine(&dir, cpus, true);
    a.runner.start();
    a.screen.wait_for("READY", Duration::from_secs(120));
    // Linux reseeds on a new generation ID only with its vmgenid driver, which takes one from a
    // device tree from 6.10 (CONFIG_VMGENID). Without it nothing reseeds the copies but Linux's own
    // timer, so what they draw first may be alike: nothing to test.
    let out = run(&a, "[ -d /sys/bus/platform/drivers/vmgenid ] || echo NO-VMGENID", "V");
    if out.contains("NO-VMGENID") {
        eprintln!("skipping: this kernel has no vmgenid driver (Linux 6.10+, CONFIG_VMGENID)");
        return;
    }
    // Linux's generator is ready (it was seeded at boot) before the snapshot is taken
    run(&a, "head -c 16 /dev/urandom > /dev/null", "W");
    let each = (0..cpus)
        .map(|c| {
            format!(
                "echo R{c}:$(taskset -c {c} head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \\n')"
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    a.runner
        .board
        .console_input(format!("echo WAITING; read -r go; {each}; echo DREW\n").as_bytes());
    a.screen.wait_for("WAITING\r\n", Duration::from_secs(120));
    let state = a.runner.save_state();

    let original = draws(&a, "DREW");
    let copies: Vec<Vec<String>> = (0..2)
        .map(|_| draws(&restored(&dir, cpus, &state), "DREW"))
        .collect();
    let all: Vec<&String> = original.iter().chain(copies.iter().flatten()).collect();
    for (i, x) in all.iter().enumerate() {
        for y in &all[i + 1..] {
            assert_ne!(
                x.split(':').nth(1),
                y.split(':').nth(1),
                "two draws alike: {all:?}"
            );
        }
    }
}

#[test]
fn fresh_randomness_one_cpu() {
    fresh_randomness(1);
}

#[test]
fn fresh_randomness_two_cpus() {
    fresh_randomness(2);
}
