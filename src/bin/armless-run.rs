//! Native runner: boots a Linux kernel (or a bare-metal binary) with the
//! serial port and the virtio console on stdio. For development and CI.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use armless::dev::{DeviceConfig, Event};
use armless::native::{HostOptions, NativeHost};
use armless::runner::{self, Runner, Stopped};
use armless::vm::{BootError, Config};

fn usage() -> ! {
    eprintln!(
        "usage: armless-run [options]
  --kernel FILE        arm64 Linux kernel (Image, Image.gz, zstd or EFI zboot)
  --initrd FILE        initramfs
  --append STRING      kernel command line (default: console=ttyS0 earlycon)
  --raw FILE[@ADDR]    bare-metal binary, loaded at ADDR (default 0x200000)
  --fs DIR             9p root filesystem: DIR/fs.json (or filesystem.json) and
                       its blobs in DIR/blobs (or DIR/filesystem), tag host9p
  --boot-fs            boot the kernel and initrd found under /boot in --fs
  --net                virtio-net, with the guest's TCP connections made from here
  --tunnel [H:]P=URL   carry the guest's TCP to H:P (any host if H is left out) over a
                       WebSocket to URL instead (repeatable; needs --net)
  --allow-private      let the guest reach loopback and private addresses (a local
                       tunnel endpoint, say)
  -m MB                RAM size in MiB (default 512)
  --smp N              number of CPUs (default 1; one thread each)
  --timeout SECS       stop after SECS seconds of wall time
  --input STRING       bytes to type into the serial port (\\n is a newline)
  --console-input S    bytes to type into the virtio console
  --console-after TEXT type them once the console printed TEXT
  --trap-undef         stop on the first undefined instruction
  --skip-idle          fast-forward time while the guest idles (1 CPU)
  --stats              print instruction count, speed and what the JIT did at exit
  --stats-json FILE    write the same as JSON to FILE (for scripts: bench/run.exs)
  --fallbacks          count, by opcode, the instructions compiled code hands back to the
                       interpreter, and print the most common (slows those down a little)"
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
    std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("armless-run: {path}: {e}");
        std::process::exit(1)
    })
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("armless-run: {msg}");
    std::process::exit(1)
}

fn main() {
    let mut kernel = None;
    let mut initrd = None;
    let mut append = "console=ttyS0 earlycon".to_string();
    let mut raw: Option<(String, u64)> = None;
    let mut fs_dir: Option<PathBuf> = None;
    let mut boot_fs = false;
    let mut net = false;
    let mut tunnels = Vec::new();
    let mut allow_private = false;
    let mut mem_mb = 512u64;
    let mut timeout = None;
    let mut input = String::new();
    let mut console_input = String::new();
    let mut console_after: Option<String> = None;
    let mut trap_undef = false;
    let mut skip_idle = false;
    let mut stats = false;
    let mut stats_json: Option<PathBuf> = None;
    let mut fallbacks = false;
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
            "--fs" => fs_dir = Some(PathBuf::from(val())),
            "--boot-fs" => boot_fs = true,
            "--net" => net = true,
            "--tunnel" => {
                tunnels.push(armless::native::Tunnel::parse(&val()).unwrap_or_else(|| usage()))
            },
            "--allow-private" => allow_private = true,
            "-m" => mem_mb = parse_num(&val()),
            "--timeout" => timeout = Some(Duration::from_secs(parse_num(&val()))),
            "--input" => input = val().replace("\\n", "\n"),
            "--console-input" => console_input = val().replace("\\n", "\n"),
            "--console-after" => console_after = Some(val()),
            "--trap-undef" => trap_undef = true,
            "--skip-idle" => skip_idle = true,
            "--stats" => stats = true,
            "--stats-json" => stats_json = Some(PathBuf::from(val())),
            "--fallbacks" => fallbacks = true,
            "--smp" => ncpus = parse_num(&val()) as usize,
            _ => usage(),
        }
    }

    let config = Config {
        memory_size: (mem_mb << 20) as usize,
        cpus: ncpus,
        devices: DeviceConfig {
            console: true,
            fs_tag: fs_dir.as_ref().map(|_| "host9p".to_string()),
            net,
            ..Default::default()
        },
    };
    let (mut vm, clock) = runner::new_vm(config).unwrap_or_else(|e| die(e));
    for c in vm.cpus.iter_mut() {
        c.trap_undefined = trap_undef;
        if fallbacks {
            c.jit.helper_hist.get_or_insert_with(Default::default);
        }
    }

    let mut blobs = None;
    if let Some(dir) = &fs_dir {
        let json = ["fs.json", "filesystem.json"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists())
            .unwrap_or_else(|| die(format!("{}: no fs.json or filesystem.json", dir.display())));
        vm.board
            .load_filesystem(&std::fs::read(&json).unwrap_or_else(|e| die(e)))
            .unwrap_or_else(|e| die(e));
        blobs = ["blobs", "filesystem"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_dir());
    }

    if let Some((file, addr)) = raw {
        armless::boot_raw(&mut vm.cpus[0], &read_file(&file), addr).unwrap_or_else(|e| die(e));
    } else if let Some(k) = kernel {
        let rd = initrd.map(|f| read_file(&f));
        vm.boot_linux(&read_file(&k), rd.as_deref(), &append)
            .unwrap_or_else(|e| die(e));
    } else if boot_fs {
        loop {
            match vm.boot_from_fs(&append) {
                Ok(_) => break,
                Err(BootError::Need(names)) => {
                    let dir = blobs.as_ref().unwrap_or_else(|| die("no blob directory"));
                    for n in names {
                        let data = std::fs::read(dir.join(&n))
                            .unwrap_or_else(|e| die(format!("{n}: {e}")));
                        vm.board.provide_blob(&n, data).unwrap_or_else(|e| die(e));
                    }
                },
                Err(BootError::Fail(e)) => die(e),
            }
        }
    } else {
        usage()
    }

    let board = vm.board.clone();
    let mut runner = Runner::new(vm, clock, skip_idle);
    let mut seen = Vec::new();
    let mut pending_input = console_after.as_ref().map(|_| console_input.clone());
    let b2 = board.clone();
    let wait_for = console_after.clone();
    let _host = NativeHost::spawn(
        board.clone(),
        HostOptions {
            blobs,
            blob_source: None,
            network: net,
            egress: armless::native::Egress {
                allow_private,
                ..armless::native::Egress::from_env()
            },
            tunnels,
        },
        Box::new(move |e| {
            let mut out = std::io::stdout();
            match e {
                Event::Serial(b) | Event::Console(b) => {
                    let _ = out.write_all(b);
                    let _ = out.flush();
                    if let (Event::Console(b), Some(after)) = (e, &wait_for) {
                        seen.extend_from_slice(b);
                        if pending_input.is_some()
                            && String::from_utf8_lossy(&seen).contains(after.as_str())
                        {
                            b2.console_input(pending_input.take().unwrap().as_bytes());
                        }
                    }
                },
                _ => {},
            }
        }),
    );
    board.serial_input(input.as_bytes());
    if console_after.is_none() {
        board.console_input(console_input.as_bytes());
    }

    let start = Instant::now();
    runner.start();
    let result = runner.wait(timeout);
    let code = match &result {
        None => {
            eprintln!("\narmless-run: timeout");
            3
        },
        Some(Stopped::PowerOff) => 0,
        Some(Stopped::Reset) => {
            eprintln!("\narmless-run: guest requested reset");
            0
        },
        Some(Stopped::Halt(c)) => *c as i32,
        Some(Stopped::Fault(f)) => {
            eprintln!("\narmless-run: stopped: {f}");
            5
        },
    };
    // Let the host thread print the last output.
    std::thread::sleep(Duration::from_millis(50));
    if stats || stats_json.is_some() || fallbacks {
        let secs = start.elapsed().as_secs_f64();
        let n = runner.insn_count();
        let cpus: Vec<CpuStats> = (0..ncpus).map(|i| runner.with_cpu(i, CpuStats::of)).collect();
        if stats || fallbacks {
            eprintln!(
                "armless-run: {n} instructions on {ncpus} CPU(s) in {secs:.2}s ({:.1} MIPS)",
                n as f64 / secs / 1e6
            );
            for (i, c) in cpus.iter().enumerate() {
                c.print(i, fallbacks);
            }
            let last = runner.with_cpu(0, |c| c.last_undefined);
            if let Some((pc, insn)) = last {
                eprintln!("armless-run: last undefined instruction {insn:#010x} at {pc:#x}");
            }
        }
        if let Some(path) = stats_json {
            let cpus: Vec<String> = cpus.iter().map(CpuStats::json).collect();
            let json = format!(
                "{{\"instructions\":{n},\"seconds\":{secs},\"cpus\":[{}]}}\n",
                cpus.join(",")
            );
            std::fs::write(&path, json).unwrap_or_else(|e| die(format!("{}: {e}", path.display())));
        }
    }
    std::process::exit(code);
}

/// What one CPU's JIT did, for `--stats`, `--stats-json` and `--fallbacks`.
struct CpuStats {
    instructions: u64,
    /// Instructions retired inside compiled code, and of those, handed back to the interpreter.
    compiled_insns: u64,
    helper_insns: u64,
    /// Floating-point vector lanes compiled code computed through the `fp3` helper.
    fp_lanes: u64,
    block_runs: u64,
    modules: u64,
    page_invalidations: u64,
    flushes: u64,
    /// Native compiler: compiles, those that failed, time spent and code held.
    native_compiles: u64,
    native_failures: u64,
    native_compile_ns: u64,
    native_code_bytes: usize,
    /// (count, an example encoding) by opcode, most common first.
    fallbacks: Vec<(u64, u32)>,
}

impl CpuStats {
    fn of(c: &mut armless::runner::NativeCpu) -> CpuStats {
        let j = &c.jit;
        let mut fallbacks: Vec<(u64, u32)> =
            j.helper_hist.iter().flat_map(|h| h.values().copied()).collect();
        fallbacks.sort_unstable_by(|a, b| b.cmp(a));
        #[cfg(feature = "native-jit")]
        let ((native_compiles, native_compile_ns), native_failures, native_code_bytes) =
            (c.native_jit_stats(), c.native_jit_failures(), c.native_jit_code_bytes());
        #[cfg(not(feature = "native-jit"))]
        let ((native_compiles, native_compile_ns), native_failures, native_code_bytes) =
            ((0, 0), 0, 0);
        CpuStats {
            instructions: c.insn_count,
            compiled_insns: j.block_insns,
            helper_insns: j.helper_interps,
            fp_lanes: j.helper_fp_lanes,
            block_runs: j.block_runs,
            modules: j.compiled,
            page_invalidations: j.page_invalidations,
            flushes: j.flushes,
            native_compiles,
            native_failures,
            native_compile_ns,
            native_code_bytes,
            fallbacks,
        }
    }

    fn print(&self, cpu: usize, fallbacks: bool) {
        let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
        eprintln!(
            "armless-run: cpu{cpu}: {} instructions, {:.1}% in compiled code ({} block runs, {:.1} \
             instructions each), {:.2}% of those handed to the interpreter, {} vector FP lanes \
             through fp3",
            self.instructions,
            pct(self.compiled_insns, self.instructions),
            self.block_runs,
            self.compiled_insns as f64 / self.block_runs.max(1) as f64,
            pct(self.helper_insns, self.compiled_insns),
            self.fp_lanes,
        );
        eprintln!(
            "armless-run: cpu{cpu}: {} modules compiled ({} natively, {} failed, {:.1} ms \
             compiling, {} KiB of code), {} pages invalidated, {} flushes",
            self.modules,
            self.native_compiles,
            self.native_failures,
            self.native_compile_ns as f64 / 1e6,
            self.native_code_bytes >> 10,
            self.page_invalidations,
            self.flushes,
        );
        if fallbacks {
            eprintln!("armless-run: cpu{cpu}: handed to the interpreter, by opcode (count, example):");
            for (n, insn) in self.fallbacks.iter().take(20) {
                eprintln!("armless-run:   {n:>12}  {insn:#010x}");
            }
        }
    }

    fn json(&self) -> String {
        let fallbacks: Vec<String> =
            self.fallbacks.iter().map(|(n, insn)| format!("[{insn},{n}]")).collect();
        format!(
            "{{\"instructions\":{},\"compiled_insns\":{},\"helper_insns\":{},\"fp_lanes\":{},\
             \"block_runs\":{},\
             \"modules\":{},\"page_invalidations\":{},\"flushes\":{},\"native_compiles\":{},\
             \"native_failures\":{},\"native_compile_ns\":{},\"native_code_bytes\":{},\
             \"fallbacks\":[{}]}}",
            self.instructions,
            self.compiled_insns,
            self.helper_insns,
            self.fp_lanes,
            self.block_runs,
            self.modules,
            self.page_invalidations,
            self.flushes,
            self.native_compiles,
            self.native_failures,
            self.native_compile_ns,
            self.native_code_bytes,
            fallbacks.join(",")
        )
    }
}
