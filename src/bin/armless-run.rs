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
  -m MB                RAM size in MiB (default 512)
  --smp N              number of CPUs (default 1; one thread each)
  --timeout SECS       stop after SECS seconds of wall time
  --input STRING       bytes to type into the serial port (\\n is a newline)
  --console-input S    bytes to type into the virtio console
  --console-after TEXT type them once the console printed TEXT
  --trap-undef         stop on the first undefined instruction
  --skip-idle          fast-forward time while the guest idles (1 CPU)
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
    let mut mem_mb = 512u64;
    let mut timeout = None;
    let mut input = String::new();
    let mut console_input = String::new();
    let mut console_after: Option<String> = None;
    let mut trap_undef = false;
    let mut skip_idle = false;
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
            "--fs" => fs_dir = Some(PathBuf::from(val())),
            "--boot-fs" => boot_fs = true,
            "--net" => net = true,
            "-m" => mem_mb = parse_num(&val()),
            "--timeout" => timeout = Some(Duration::from_secs(parse_num(&val()))),
            "--input" => input = val().replace("\\n", "\n"),
            "--console-input" => console_input = val().replace("\\n", "\n"),
            "--console-after" => console_after = Some(val()),
            "--trap-undef" => trap_undef = true,
            "--skip-idle" => skip_idle = true,
            "--stats" => stats = true,
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
        HostOptions { blobs, nat: net },
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
    if stats {
        let secs = start.elapsed().as_secs_f64();
        let n = runner.insn_count();
        eprintln!(
            "armless-run: {n} instructions on {ncpus} CPU(s) in {secs:.2}s ({:.1} MIPS)",
            n as f64 / secs / 1e6
        );
        let last = runner.with_cpu(0, |c| c.last_undefined);
        if let Some((pc, insn)) = last {
            eprintln!("armless-run: last undefined instruction {insn:#010x} at {pc:#x}");
        }
    }
    std::process::exit(code);
}
