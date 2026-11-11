//! armless-exec: run a static aarch64 Linux ELF on armless with no guest kernel (`armless_user`),
//! and report what the machine holds. For experiments and measurements.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use armless::vm::Config;
use armless::Ram;
use armless_user::{Process, Status};

fn usage() -> ! {
    eprintln!(
        "usage: armless-exec [options] ELF [ARGS...]
       armless-exec [options] --restore DIR
  -m MB              RAM (default 1024; untouched RAM costs nothing)
  --stop-after TEXT  stop once the program printed TEXT
  --save DIR         when it stops, write the machine (machine.bin) and the process (process.bin)
  --restore DIR      start from what --save wrote instead of an ELF
  --input LINE       type LINE (and a newline) into its stdin, and stop at the next line it prints
  --timeout SECS     stop after SECS of wall time
  ARMLESS_EXEC_TRACE=1 prints every syscall"
    );
    std::process::exit(2)
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("armless-exec: {msg}");
    std::process::exit(1)
}

fn main() {
    let start = Instant::now();
    let mut mem_mb = 1024u64;
    let mut stop_after: Option<String> = None;
    let mut save: Option<PathBuf> = None;
    let mut restore: Option<PathBuf> = None;
    let mut input: Option<String> = None;
    let mut timeout = Duration::from_secs(60);
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.peek().cloned() {
        if !a.starts_with('-') {
            break;
        }
        args.next();
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "-m" => mem_mb = val().parse().unwrap_or_else(|_| usage()),
            "--stop-after" => stop_after = Some(val()),
            "--save" => save = Some(PathBuf::from(val())),
            "--restore" => restore = Some(PathBuf::from(val())),
            "--input" => input = Some(val()),
            "--timeout" => timeout = Duration::from_secs(val().parse().unwrap_or_else(|_| usage())),
            _ => usage(),
        }
    }
    let guest_args: Vec<String> = args.collect();
    if guest_args.is_empty() == restore.is_none() {
        usage();
    }

    // RAM on a private mapping of a sparse file (all holes, so all zero), not calloc's: the
    // process can then give the host back the pages the program unmaps.
    let config = Config {
        memory_size: (mem_mb << 20) as usize,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            ..Default::default()
        },
    };
    let backing = std::env::temp_dir().join(format!("armless-exec-{}.ram", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&backing)
        .unwrap_or_else(|e| die(e));
    file.set_len(mem_mb << 20).unwrap_or_else(|e| die(e));
    let _ = std::fs::remove_file(&backing);
    let ram = Ram::from_image(&file, (mem_mb << 20) as usize).unwrap_or_else(|e| die(e));
    let (mut vm, _clock) = armless::runner::new_vm_on(config, Some(ram)).unwrap_or_else(|e| die(e));

    let mut p = match &restore {
        Some(dir) => {
            let machine = std::fs::read(dir.join("machine.bin")).unwrap_or_else(|e| die(e));
            vm.load_state(&machine).unwrap_or_else(|e| die(e));
            let process = std::fs::read(dir.join("process.bin")).unwrap_or_else(|e| die(e));
            Process::restore(&mut vm.cpus[0], &process).unwrap_or_else(|e| die(e))
        },
        None => {
            let elf = std::fs::read(&guest_args[0])
                .unwrap_or_else(|e| die(format!("{}: {e}", guest_args[0])));
            let env = ["PATH=/bin".to_string(), "HOME=/".to_string()];
            Process::exec(&mut vm.cpus[0], &elf, &guest_args, &env).unwrap_or_else(|e| die(e))
        },
    };
    let ready = start.elapsed();
    p.trace = std::env::var_os("ARMLESS_EXEC_TRACE").is_some();
    let lines_before = input.as_ref().map(|line| {
        p.input(format!("{line}\n").as_bytes());
        0usize
    });

    // Run, forwarding its output, until it exits, faults, prints what we wait for, or times out
    let cpu = &mut vm.cpus[0];
    let mut printed = Vec::new();
    let mut first_output = None;
    let mut stopped_at = None;
    let status = loop {
        let status = p.run(cpu, Duration::from_millis(20));
        let out = p.take_output();
        if !out.is_empty() {
            first_output.get_or_insert(start.elapsed());
            let mut o = std::io::stdout();
            let _ = o.write_all(&out);
            let _ = o.flush();
            printed.extend_from_slice(&out);
        }
        let seen = stop_after
            .as_ref()
            .is_some_and(|t| printed.windows(t.len()).any(|w| w == t.as_bytes()))
            || lines_before.is_some_and(|n| printed.iter().filter(|&&b| b == b'\n').count() > n);
        if seen {
            stopped_at = Some(start.elapsed());
            break status;
        }
        match status {
            Status::Running => {},
            Status::Idle(wake) => {
                let wait = wake.map_or(Duration::from_millis(20), |w| {
                    w.saturating_duration_since(Instant::now())
                });
                std::thread::sleep(wait.min(Duration::from_millis(20)));
            },
            Status::Exited(_) | Status::Fault(_) => break status,
        }
        if start.elapsed() > timeout {
            eprintln!("armless-exec: timed out");
            break status;
        }
    };
    p.park(cpu);
    let insns = cpu.insn_count;
    let footprint = footprint();
    let threads = p.threads();
    let stats = std::mem::take(&mut p.stats);

    let saved = save.as_ref().map(|dir| {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| die(e));
        let process = p.save(&vm.cpus[0]);
        let machine = vm.save_state();
        std::fs::write(dir.join("machine.bin"), &machine).unwrap_or_else(|e| die(e));
        std::fs::write(dir.join("process.bin"), &process).unwrap_or_else(|e| die(e));
        (machine.len(), process.len())
    });

    eprintln!("--- armless-exec report");
    eprintln!("status: {status:?}");
    eprintln!("ready_ms: {:.2}", ready.as_secs_f64() * 1e3);
    eprintln!(
        "first_output_ms: {:?}",
        first_output.map(|d| d.as_secs_f64() * 1e3)
    );
    eprintln!(
        "stopped_ms: {:?}",
        stopped_at.map(|d| d.as_secs_f64() * 1e3)
    );
    eprintln!("instructions: {insns}");
    eprintln!("threads: {threads} (switches {})", stats.switches);
    eprintln!("host_footprint_mib: {:.1}", footprint as f64 / 1048576.0);
    eprintln!("saved_bytes (machine, process): {saved:?}");
    eprintln!("guest_pages_nonzero: {}", p.guest_pages());
    eprintln!("signals_dropped: {}", stats.signals_dropped);
    eprintln!("syscalls: {:?}", stats.syscalls);
    eprintln!("unknown_syscalls: {:?}", stats.unknown);
    eprintln!("opens: {:?}", stats.opens);
    std::process::exit(match status {
        Status::Exited(c) => c,
        Status::Fault(_) => 139,
        _ => 0,
    })
}

/// The process's physical footprint (macOS), what snowglobe measures machines by.
#[cfg(target_os = "macos")]
fn footprint() -> u64 {
    extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    let mut info = [0u64; 64];
    // SAFETY: RUSAGE_INFO_V2 (2) fills a rusage_info_v2, smaller than the buffer.
    unsafe { proc_pid_rusage(std::process::id() as i32, 2, info.as_mut_ptr()) };
    info[9] // ri_phys_footprint: after the 16-byte uuid, the 8th u64
}

#[cfg(not(target_os = "macos"))]
fn footprint() -> u64 {
    0
}
