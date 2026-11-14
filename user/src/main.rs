//! armless-exec: run a static aarch64 Linux ELF on armless with no guest kernel (`armless_user`),
//! and report what the machine holds. For experiments and measurements.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use armless::vm::Config;
use armless::Ram;
use armless_user::{Process, Smp, Status};

fn usage() -> ! {
    eprintln!(
        "usage: armless-exec [options] PROGRAM [ARGS...]
       armless-exec [options] --restore DIR
  --fs DIR           the machine's filesystem: DIR/fs.json (or filesystem.json) and its blobs in
                     DIR/blobs (or DIR/filesystem), a snowglobe site's system/ say. PROGRAM is then
                     found in it, as execve finds it (PATH when it has no slash); without --fs it
                     is a static executable of this host's files
  --cwd DIR          the working directory in that filesystem (default /)
  --env NAME=VALUE   an environment variable (repeatable; default PATH, HOME and TERM)
  --keys TEXT        type TEXT into its terminal once it waits for input (\\n, \\r and \\x03 escapes)
  --spawn CMD        once it waits, run /bin/sh -c CMD beside it as a process of its own (a job of
                     snowglobe's), print what it printed and its status, and stop
  -m MB              RAM (default 1024; untouched RAM costs nothing)
  --cpus N           CPUs (default 1, at most 8): threads run on them at the same time
  --stop-after TEXT  stop once the program printed TEXT
  --save DIR         when it stops, write the machine (machine.bin) and the process (process.bin)
  --restore DIR      start from what --save wrote instead of an ELF
  --input LINE       type LINE (and a newline) into its stdin, and stop at the next line it prints
  --timeout SECS     stop after SECS of wall time
  --net              a network: connections to addresses that are not the loopback's are made
                     from here, and what the program listens on can be published
  --publish [ADDR:]HOSTPORT:PORT
                     listen on this host's ADDR:HOSTPORT (default 127.0.0.1) and carry each
                     connection to the program's TCP PORT (repeatable; needs --net)
  --tunnel [H:]P=URL carry the program's TCP to H:P over a WebSocket to URL (ws://, wss://) or
                     a TCP connection to it (tcp://HOST:PORT) instead (repeatable; needs --net)
  --allow-private    let the program reach loopback, private and link-local addresses at
                     this host (a local tunnel endpoint, say)
  ARMLESS_EXEC_TRACE=1 prints every syscall, ARMLESS_EXEC_TRACE_NR=131,133 only those"
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
    let mut cpus = 1usize;
    let mut stop_after: Option<String> = None;
    let mut save: Option<PathBuf> = None;
    let mut restore: Option<PathBuf> = None;
    let mut input: Option<String> = None;
    let mut timeout = Duration::from_secs(60);
    let mut fs_dir: Option<PathBuf> = None;
    let mut cwd = "/".to_string();
    let mut env: Vec<String> = Vec::new();
    let mut keys: Option<String> = None;
    let mut spawn: Option<String> = None;
    let mut net = false;
    let mut publish: Vec<(String, u16)> = Vec::new();
    let mut tunnels = Vec::new();
    let mut allow_private = false;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.peek().cloned() {
        if !a.starts_with('-') {
            break;
        }
        args.next();
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "-m" => mem_mb = val().parse().unwrap_or_else(|_| usage()),
            "--cpus" => {
                cpus = val().parse().unwrap_or_else(|_| usage());
                if !(1..=8).contains(&cpus) {
                    usage()
                }
            },
            "--stop-after" => stop_after = Some(val()),
            "--save" => save = Some(PathBuf::from(val())),
            "--restore" => restore = Some(PathBuf::from(val())),
            "--input" => input = Some(val()),
            "--timeout" => timeout = Duration::from_secs(val().parse().unwrap_or_else(|_| usage())),
            "--fs" => fs_dir = Some(PathBuf::from(val())),
            "--cwd" => cwd = val(),
            "--net" => net = true,
            "--publish" => {
                // [ADDR:]HOSTPORT:PORT
                let v = val();
                let (host, port) = v.rsplit_once(':').unwrap_or_else(|| usage());
                let port: u16 = port.parse().unwrap_or_else(|_| usage());
                let host =
                    if host.contains(':') { host.to_string() } else { format!("127.0.0.1:{host}") };
                publish.push((host, port));
            },
            "--tunnel" => {
                tunnels.push(armless::native::Tunnel::parse(&val()).unwrap_or_else(|| usage()))
            },
            "--allow-private" => allow_private = true,
            "--env" => env.push(val()),
            "--spawn" => spawn = Some(val()),
            "--keys" => {
                keys = Some(
                    val()
                        .replace("\\n", "\n")
                        .replace("\\r", "\r")
                        .replace("\\x03", "\x03"),
                )
            },
            _ => usage(),
        }
    }
    let guest_args: Vec<String> = args.collect();
    if guest_args.is_empty() == restore.is_none() {
        usage();
    }

    // RAM on an anonymous mapping of its own, not calloc's: the process can then give the host
    // back the pages the program unmaps, and pages it never writes cost nothing.
    let config = Config {
        memory_size: (mem_mb << 20) as usize,
        cpus,
        devices: armless::dev::DeviceConfig {
            console: false,
            net,
            fs_tag: fs_dir.as_ref().map(|_| "host9p".to_string()),
            ..Default::default()
        },
    };
    let ram = Ram::mapped((mem_mb << 20) as usize).unwrap_or_else(|e| die(e));
    let (mut vm, _clock) = armless::runner::new_vm_on(config, Some(ram)).unwrap_or_else(|e| die(e));

    // The filesystem, its blobs served from the directory by armless's native host
    let mut host: Option<Arc<armless::native::NativeHost>> = None;
    let blobs = fs_dir.as_ref().and_then(|dir| {
        ["blobs", "filesystem"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_dir())
    });
    if let Some(dir) = &fs_dir {
        let json = ["fs.json", "filesystem.json"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists())
            .unwrap_or_else(|| die("no fs.json or filesystem.json"));
        vm.board
            .load_filesystem(&std::fs::read(&json).unwrap_or_else(|e| die(e)))
            .unwrap_or_else(|e| die(e));
    }
    // The host's side of the machine: its blobs, its network
    if fs_dir.is_some() || net {
        host = Some(Arc::new(armless::native::NativeHost::spawn(
            vm.board.clone(),
            armless::native::HostOptions {
                blobs,
                network: net,
                egress: armless::native::Egress {
                    allow_private,
                    ..armless::native::Egress::from_env()
                },
                tunnels,
                ..Default::default()
            },
            Box::new(|_| {}),
        )));
    }
    for (addr, port) in &publish {
        let (Some(host), true) = (&host, net) else {
            die("--publish needs --net")
        };
        // A port that is taken is said now; the host's side is opened once the program listens
        // (a connect that succeeds says something is there to answer, as a wait for a port needs)
        drop(std::net::TcpListener::bind(addr).unwrap_or_else(|e| die(format!("{addr}: {e}"))));
        let (host, addr, port, board) = (host.clone(), addr.clone(), *port, vm.board.clone());
        std::thread::spawn(move || {
            while !board.local_listening(port) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let Ok(listener) = std::net::TcpListener::bind(&addr) else {
                return eprintln!("{addr}: taken since the start");
            };
            for stream in listener.incoming().flatten() {
                host.forward(stream, port);
            }
        });
    }
    let board = (fs_dir.is_some() || net).then(|| vm.board.clone());
    if env.is_empty() {
        env = [
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "HOME=/root",
            "TERM=xterm-256color",
            "LANG=C.UTF-8",
        ]
        .map(String::from)
        .to_vec();
    }

    let mut p = match &restore {
        Some(dir) => {
            let machine = std::fs::read(dir.join("machine.bin")).unwrap_or_else(|e| die(e));
            vm.load_state(&machine).unwrap_or_else(|e| die(e));
            let process = std::fs::read(dir.join("process.bin")).unwrap_or_else(|e| die(e));
            Process::restore(&mut vm.cpus[0], board, &process).unwrap_or_else(|e| die(e))
        },
        None => match board {
            // Found in the filesystem; its blobs (and its loader's) come from the host meanwhile
            Some(board) if fs_dir.is_some() => loop {
                match Process::exec(&mut vm.cpus[0], board.clone(), &guest_args, &env, &cwd) {
                    Ok(p) => break p,
                    Err(armless_user::ExecError::Need(_)) if start.elapsed() < timeout => {
                        std::thread::sleep(Duration::from_millis(5))
                    },
                    Err(e) => die(e),
                }
            },
            board => {
                let elf = std::fs::read(&guest_args[0])
                    .unwrap_or_else(|e| die(format!("{}: {e}", guest_args[0])));
                Process::exec_bytes(&mut vm.cpus[0], board, &elf, &guest_args, &env)
                    .unwrap_or_else(|e| die(e))
            },
        },
    };
    let ready = start.elapsed();
    p.trace = std::env::var_os("ARMLESS_EXEC_TRACE").is_some();
    // ARMLESS_EXEC_TRACE_NR=129,131,133: only those system calls
    if let Ok(nrs) = std::env::var("ARMLESS_EXEC_TRACE_NR") {
        p.trace = true;
        p.trace_only = nrs
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
    }
    // The line, and its echo, which isn't the answer waited for
    let mut printed = Vec::new();
    let lines_before = input.as_ref().map(|line| {
        p.input(format!("{line}\n").as_bytes());
        printed.extend(p.take_output());
        printed.iter().filter(|&&b| b == b'\n').count()
    });

    // Run, forwarding its output, until it exits, faults, prints what we wait for, or times out:
    // CPU 0 on this thread, the others on threads of their own
    let smp = Smp::new(&mut p);
    let mut first_output = None;
    let mut stopped_at = None;
    let (cpu0, others) = vm.cpus.split_at_mut(1);
    let workers = others.len();
    let cpu = &mut cpu0[0];
    let status = std::thread::scope(|s| {
        for (i, c) in others.iter_mut().enumerate() {
            let smp = &smp;
            s.spawn(move || smp.serve(i + 1, c));
        }
        let status = loop {
            let status = smp.run(0, cpu, Duration::from_millis(20));
            let out = smp.lock().take_output();
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
                || lines_before
                    .is_some_and(|n| printed.iter().filter(|&&b| b == b'\n').count() > n);
            if seen {
                stopped_at = Some(start.elapsed());
                break status;
            }
            match status {
                Status::Running => {},
                Status::Idle(_) if spawn.is_some() => {
                    let cmd = spawn.take().unwrap();
                    let argv = ["/bin/sh".to_string(), "-c".into(), cmd];
                    let pid = loop {
                        match smp.lock().spawn(&argv, &env, &cwd, None) {
                            Ok(pid) => break pid,
                            Err(armless_user::ExecError::Need(_)) => {
                                std::thread::sleep(Duration::from_millis(5))
                            },
                            Err(e) => die(e),
                        }
                    };
                    smp.lock().spawn_close_input(pid);
                    let exit = loop {
                        smp.run(0, cpu, Duration::from_millis(20));
                        let r = smp.lock().spawn_poll(pid).unwrap();
                        let _ = std::io::stdout().write_all(&r.stdout);
                        let _ = std::io::stderr().write_all(&r.stderr);
                        if let Some(e) = r.exit {
                            break e;
                        }
                        if start.elapsed() > timeout {
                            die("the spawned command timed out")
                        }
                    };
                    smp.lock().spawn_release(pid);
                    eprintln!("armless-exec: spawned command exited {exit}");
                    break status;
                },
                Status::Idle(_) if keys.is_some() => {
                    smp.lock().input(keys.take().unwrap().as_bytes());
                    smp.poke();
                },
                // Blobs come from the host thread soon
                Status::Idle(_) if smp.lock().waiting_on_blobs() => {
                    std::thread::sleep(Duration::from_millis(1))
                },
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
        // The other CPUs' threads back in the process, for a snapshot to have them
        smp.pause(workers);
        if std::env::var_os("ARMLESS_EXEC_WAITS").is_some() {
            eprint!("--- waits\n{}", smp.lock().describe_waits());
        }
        for (i, t) in smp.times().iter().enumerate().take(workers + 1) {
            eprintln!(
                "cpu{i}: guest {:.2}s, waiting for the lock {:.2}s, holding it {:.2}s",
                t[0] as f64 / 1e9,
                t[1] as f64 / 1e9,
                t[2] as f64 / 1e9
            );
        }
        smp.stop();
        status
    });
    drop(smp);
    p.park(&vm.cpus[0]);
    let insns: u64 = vm.cpus.iter().map(|c| c.insn_count).sum();
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
    eprintln!(
        "instructions: {insns} (per cpu {:?})",
        vm.cpus.iter().map(|c| c.insn_count).collect::<Vec<_>>()
    );
    eprintln!("threads: {threads} (switches {})", stats.switches);
    eprintln!("host_footprint_mib: {:.1}", footprint as f64 / 1048576.0);
    eprintln!("saved_bytes (machine, process): {saved:?}");
    eprintln!("guest_pages_nonzero: {}", p.guest_pages());
    eprintln!("lazy_pages (left, filled): {:?}", p.lazy_pages());
    eprintln!(
        "signals (delivered, ignored): {} {}",
        stats.signals_delivered, stats.signals_ignored
    );
    eprintln!("syscalls: {:?}", stats.syscalls);
    eprintln!("unknown_syscalls: {:?}", stats.unknown);
    eprintln!(
        "faults (processes that died of SIGSEGV): {:?}",
        stats.faults
    );
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
