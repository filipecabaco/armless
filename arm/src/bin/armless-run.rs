//! Native runner: boots a Linux Image or a raw bare-metal binary with the UART
//! on stdio. Used for development and CI; the browser build uses v86's JS.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use armless::cpu::{Cpu, Exit, StopReason};
use armless::devices::{NativeBus, SharedBus};
use armless::smp::{self, Machine};

fn usage() -> ! {
    eprintln!(
        "usage: armless-run [options]
  --kernel FILE        arm64 Linux Image (uncompressed)
  --initrd FILE        initramfs
  --append STRING      kernel command line (default: console=ttyS0 earlycon)
  --raw FILE[@ADDR]    bare-metal binary, loaded at ADDR (default 0x200000)
  -m MB                RAM size in MiB (default 512)
  --smp N              number of CPUs (default 1; N > 1 runs one thread per CPU)
  --max-insns N        stop after N instructions
  --timeout SECS       stop after SECS seconds of wall time
  --input STRING       bytes to feed to the UART (\\n is a newline)
  --trap-undef         stop on the first undefined instruction
  --stats              print instruction count and speed at exit"
    );
    std::process::exit(2);
}

fn parse_num(s: &str) -> u64 {
    if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16).unwrap_or_else(|_| usage())
    } else {
        s.parse().unwrap_or_else(|_| usage())
    }
}

fn read_file(path: &str) -> Vec<u8> {
    let mut v = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut v))
        .unwrap_or_else(|e| {
            eprintln!("armless-run: {path}: {e}");
            std::process::exit(1)
        });
    v
}

fn main() {
    let mut kernel = None;
    let mut initrd = None;
    let mut append = "console=ttyS0 earlycon".to_string();
    let mut raw: Option<(String, u64)> = None;
    let mut mem_mb = 512u64;
    let mut max_insns = u64::MAX;
    let mut timeout = None;
    let mut input = String::new();
    let mut trap_undef = false;
    let mut stats = false;
    let mut ncpus = 1usize;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--kernel" => kernel = Some(val()),
            "--initrd" => initrd = Some(val()),
            "--append" => append = val(),
            "--raw" => {
                let v = val();
                raw = Some(match v.split_once('@') {
                    Some((f, addr)) => (f.to_string(), parse_num(addr)),
                    None => (v, armless::board::KERNEL_OFFSET),
                });
            },
            "-m" => mem_mb = parse_num(&val()),
            "--max-insns" => max_insns = parse_num(&val()),
            "--timeout" => timeout = Some(Duration::from_secs(parse_num(&val()))),
            "--input" => input = val().replace("\\n", "\n"),
            "--trap-undef" => trap_undef = true,
            "--stats" => stats = true,
            "--smp" => ncpus = parse_num(&val()) as usize,
            _ => usage(),
        }
    }

    if ncpus > 1 {
        run_smp(
            ncpus, mem_mb, kernel, initrd, &append, raw, timeout, &input, trap_undef, stats,
        );
    }
    let mut cpu = Cpu::new((mem_mb << 20) as usize, NativeBus::new());
    cpu.trap_undefined = trap_undef;
    cpu.host.uart.input.extend(input.bytes());
    let layout = if let Some((file, addr)) = raw {
        armless::boot_raw(&mut cpu, &read_file(&file), addr)
    } else if let Some(k) = kernel {
        let rd = initrd.map(|f| read_file(&f));
        armless::boot_linux(&mut cpu, &read_file(&k), rd.as_deref(), &append)
    } else {
        usage()
    };
    if let Err(e) = layout {
        eprintln!("armless-run: {e}");
        std::process::exit(1);
    }

    let start = Instant::now();
    let mut stdout = std::io::stdout();
    let code = loop {
        let budget = (max_insns - cpu.insn_count).min(100_000);
        if budget == 0 {
            eprintln!(
                "\narmless-run: instruction limit reached at pc={:#x}",
                cpu.pc
            );
            break 3;
        }
        let exit = cpu.run(budget);
        let out = cpu.host.take_output();
        if !out.is_empty() {
            let _ = stdout.write_all(&out);
            let _ = stdout.flush();
        }
        if timeout.is_some_and(|t| start.elapsed() > t) {
            eprintln!("\narmless-run: timeout at pc={:#x}", cpu.pc);
            break 3;
        }
        match exit {
            Exit::Budget => {},
            Exit::Idle { wake_ns } => {
                // Nothing to do until the timer fires: skip ahead in time
                // instead of sleeping, so idle loops don't slow tests down.
                if let Some(w) = wake_ns {
                    let host_now = armless::board::Host::now_ns(&mut cpu.host);
                    if w > host_now {
                        cpu.host.skew_ns += w - host_now;
                    }
                } else if cpu.host.uart.input.is_empty() {
                    eprintln!(
                        "\narmless-run: guest idle with no wakeup source at pc={:#x}",
                        cpu.pc
                    );
                    break 4;
                }
            },
            Exit::Stop(StopReason::PowerOff) => break 0,
            Exit::Stop(StopReason::Reset) => {
                eprintln!("\narmless-run: guest requested reset");
                break 0;
            },
            Exit::Stop(StopReason::Halt(code)) => break code as i32,
            Exit::Stop(r) => {
                eprintln!("\narmless-run: stopped: {r:x?}");
                eprintln!(
                    "pc={:#x} el={} sp={:#x} lr={:#x} esr={:#x} far={:#x} elr={:#x}",
                    cpu.pc,
                    cpu.el,
                    cpu.sp(),
                    cpu.x[30],
                    cpu.sys.esr,
                    cpu.sys.far,
                    cpu.sys.elr
                );
                break 5;
            },
        }
    };
    if stats {
        let secs = start.elapsed().as_secs_f64();
        eprintln!(
            "armless-run: {} instructions in {:.2}s ({:.1} MIPS)",
            cpu.insn_count,
            secs,
            cpu.insn_count as f64 / secs / 1e6
        );
    }
    if let Some((pc, insn)) = cpu.last_undefined {
        if stats {
            eprintln!("armless-run: last undefined instruction {insn:#010x} at {pc:#x}");
        }
    }
    std::process::exit(code);
}

/// One thread per CPU, sharing RAM, the GIC and the UART.
#[allow(clippy::too_many_arguments)]
fn run_smp(
    ncpus: usize,
    mem_mb: u64,
    kernel: Option<String>,
    initrd: Option<String>,
    append: &str,
    raw: Option<(String, u64)>,
    timeout: Option<Duration>,
    input: &str,
    trap_undef: bool,
    stats: bool,
) -> ! {
    if !(1..=smp::MAX_CPUS).contains(&ncpus) {
        eprintln!("armless-run: --smp must be 1..={}", smp::MAX_CPUS);
        std::process::exit(2);
    }
    let machine = Arc::new(Machine::new((mem_mb << 20) as usize, ncpus));
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    machine.host_cores.store(cores as u32, Ordering::Relaxed);
    let bus = SharedBus::new(NativeBus::new());
    bus.0.lock().unwrap().uart.input.extend(input.bytes());

    // Idle CPUs sleep on their condvar until their timer deadline or a kick.
    let sleepers: Arc<Vec<(Mutex<()>, Condvar)>> = Arc::new(
        (0..ncpus)
            .map(|_| (Mutex::new(()), Condvar::new()))
            .collect(),
    );
    {
        let sleepers = sleepers.clone();
        machine.set_waker(Box::new(move |cpu| {
            let (m, cv) = &sleepers[cpu];
            let _g = m.lock().unwrap();
            cv.notify_all();
        }));
    }

    let mut cpus: Vec<Cpu<SharedBus>> = (0..ncpus)
        .map(|i| Cpu::new_smp(machine.clone(), i, bus.clone()))
        .collect();
    for c in cpus.iter_mut() {
        c.trap_undefined = trap_undef;
    }
    let layout = if let Some((file, addr)) = raw {
        armless::boot_raw(&mut cpus[0], &read_file(&file), addr)
    } else if let Some(k) = kernel {
        let rd = initrd.map(|f| read_file(&f));
        armless::boot_linux(&mut cpus[0], &read_file(&k), rd.as_deref(), append)
    } else {
        usage()
    };
    if let Err(e) = layout {
        eprintln!("armless-run: {e}");
        std::process::exit(1);
    }

    let start = Instant::now();
    let code = Arc::new(AtomicI32::new(i32::MIN));
    let total = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = cpus
        .into_iter()
        .map(|mut cpu| {
            let machine = machine.clone();
            let sleepers = sleepers.clone();
            let code = code.clone();
            let total = total.clone();
            let bus = bus.clone();
            std::thread::spawn(move || {
                let i = cpu.index;
                let set_code = |c: i32| {
                    let _ = code.compare_exchange(i32::MIN, c, Ordering::SeqCst, Ordering::SeqCst);
                };
                loop {
                    let woken = machine.cpus[i].wake.load(Ordering::SeqCst);
                    let exit = cpu.run(100_000);
                    let out = bus.0.lock().unwrap().take_output();
                    if !out.is_empty() {
                        let mut stdout = std::io::stdout();
                        let _ = stdout.write_all(&out);
                        let _ = stdout.flush();
                    }
                    if timeout.is_some_and(|t| start.elapsed() > t) {
                        eprintln!("\narmless-run: timeout (cpu {i} at pc={:#x})", cpu.pc);
                        set_code(3);
                        machine.request_stop(smp::STOP_POWER_OFF);
                    }
                    match exit {
                        Exit::Budget => {},
                        Exit::Idle { wake_ns } => {
                            let (m, cv) = &sleepers[i];
                            let mut g = m.lock().unwrap();
                            let now = armless::board::Host::now_ns(&mut cpu.host);
                            let wait = match wake_ns {
                                Some(w) => Duration::from_nanos(w.saturating_sub(now)),
                                None => Duration::from_millis(50),
                            };
                            let deadline = Instant::now() + wait;
                            while machine.cpus[i].wake.load(Ordering::SeqCst) == woken {
                                let left = deadline.saturating_duration_since(Instant::now());
                                if left.is_zero() {
                                    break;
                                }
                                g = cv.wait_timeout(g, left).unwrap().0;
                            }
                        },
                        Exit::Stop(StopReason::PowerOff) => {
                            set_code(0);
                            machine.request_stop(smp::STOP_POWER_OFF);
                            break;
                        },
                        Exit::Stop(StopReason::Reset) => {
                            eprintln!("\narmless-run: guest requested reset");
                            set_code(0);
                            machine.request_stop(smp::STOP_RESET);
                            break;
                        },
                        Exit::Stop(StopReason::Halt(c)) => {
                            set_code(c as i32);
                            machine.request_stop(smp::STOP_POWER_OFF);
                            break;
                        },
                        Exit::Stop(r) => {
                            eprintln!("\narmless-run: cpu {i} stopped: {r:x?}");
                            eprintln!(
                                "pc={:#x} el={} sp={:#x} lr={:#x} esr={:#x} far={:#x} elr={:#x}",
                                cpu.pc,
                                cpu.el,
                                cpu.sp(),
                                cpu.x[30],
                                cpu.sys.esr,
                                cpu.sys.far,
                                cpu.sys.elr
                            );
                            set_code(5);
                            machine.request_stop(smp::STOP_POWER_OFF);
                            break;
                        },
                    }
                }
                total.fetch_add(cpu.insn_count, Ordering::SeqCst);
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    let out = bus.0.lock().unwrap().take_output();
    let _ = std::io::stdout().write_all(&out);
    if stats {
        let secs = start.elapsed().as_secs_f64();
        let n = total.load(Ordering::SeqCst);
        eprintln!(
            "armless-run: {n} instructions on {ncpus} CPUs in {secs:.2}s ({:.1} MIPS)",
            n as f64 / secs / 1e6
        );
    }
    std::process::exit(code.load(Ordering::SeqCst).max(0));
}
