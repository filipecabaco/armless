# armless

An AArch64 machine emulator written in Rust. It boots unmodified arm64 Linux, in a
browser tab (WebAssembly, driven by [`js/armless.js`](js/armless.js)) and natively
(a Rust crate and the `armless-run` command). The whole machine, devices included, is
Rust; the host only provides time, file contents and the other end of network connections.

[snowglobe](https://github.com/filipecabaco/snowglobe) uses armless to turn Docker images
into static sites that restore a running app in the visitor's browser in about a second.
armless started as a fork of [v86](https://github.com/copy/v86).

## What it has

- **CPU:** ARMv8-A at EL0 and EL1 with FP/AdvSIMD, AES/PMULL, SHA1, SHA256, CRC32 and the
  ARMv8.1 atomics (LSE). A software MMU with a TLB, and up to 8 CPUs, one Web Worker or
  native thread each.
- **Execution backends:** an interpreter; a JIT that turns hot pages into WebAssembly
  modules (browser) or host code with Cranelift (native); and, opt in, hardware
  virtualization (Hypervisor.framework on Apple silicon macOS, KVM on Linux arm64).
- **Interrupts and time:** GICv2, the generic timer, a PL031 RTC, and a VM generation ID
  so a restored guest reseeds its random generator.
- **Devices:** a 16550 UART (`ttyS0`), and virtio-pci devices: console (`hvc0`, with the
  terminal size), 9p filesystem, and network.
- **Filesystem:** a 9p root built from content-addressed blobs, fetched on first read.
  Large files are split into chunks, so a read fetches only what it covers.
- **Network:** virtio-net with an in-process TCP/IP stack. DHCP, DNS and ping are answered
  locally. HTTP and HTTPS (ports 80 and 443; HTTPS ends in the machine with a CA the guest
  trusts) and WebSockets reach the host as requests to make: a page answers them with
  `fetch()` and `WebSocket`, a native host with its own HTTP client, proxies included.
  Other TCP connections become streams the host accepts, refuses, or carries over a
  WebSocket tunnel.
- **Snapshots:** the whole machine can be saved and restored. A restore streams straight
  into guest memory, zstd-compressed or not, and a save is handed out in pieces.
- **User mode (experimental):** a Linux program runs with no guest kernel at all
  ([`user/`](user)): the CPU runs it at EL0 and its system calls are answered by the host.
  Static and dynamically linked programs, the machine's filesystem, a terminal, threads, pipes,
  epoll, poll and select, signals, and snapshots of the running process work; processes and
  sockets do not yet. Go, Python 3.14 and DuckDB run this way from their own images,
  natively and in the browser (a snapshot made on one restores on the other).

## Quick start

### Native

```sh
cargo build --release
cargo run --release --bin armless-run -- --kernel Image --initrd initrd.cpio --smp 2
cargo run --release --bin armless-run -- --help
```

`armless-run` runs Linux kernels (`Image`, `Image.gz`, zstd or EFI zboot) and bare-metal
binaries (`--raw FILE[@ADDR]`). Besides `--kernel`, `--initrd`, `--append`, `-m MB` and
`--smp N`, it takes `--fs DIR` (a 9p root; `--boot-fs` boots the kernel under `/boot` in
it), `--net` with `--tunnel [H:]P=URL` and `--allow-private`, `--timeout`, input flags for
scripted runs, and `--stats` / `--fallbacks` for JIT numbers.

Rust 1.97 or newer is needed (Cranelift, behind the default `native-jit` feature).

`armless-exec` runs a Linux program in user mode, with no kernel: a static executable of this
host's, or (`--fs DIR`, a snowglobe site's `system/` say) one found in a machine's filesystem:

```sh
cargo run --release --bin armless-exec -- ./my-static-binary arg1 arg2
cargo run --release --bin armless-exec -- --fs site/system --keys 'print(6*7)\n' python3
cargo run --release --bin armless-exec -- --stop-after "ready" --save snap ./server
cargo run --release --bin armless-exec -- --restore snap --input "a line for its terminal"
```

### Browser

Build `armless.wasm` (see [Building](#building)) or take the files from a
[release](#releases), then:

```html
<script src="armless.js"></script>
<script>
  const vm = new Armless({
    wasm: "armless.wasm",            // armless-smp.wasm next to it is used for cpus > 1
    memory_mb: 512,
    cpus: 1,
    filesystem: { json: "filesystem.json", baseurl: "filesystem/" },
    initial_state: "state.bin.zst",  // or boot: { kernel, initrd, cmdline }
    network: { tls: "tls.json" },    // the guest's HTTP(S) and WebSockets go out via fetch()
  });
  vm.on("console", (bytes) => term.write(bytes));
  term.onData((text) => vm.console_input(text));
  vm.on("ready", () => vm.console_resize(term.cols, term.rows));
</script>
```

More than one CPU needs `SharedArrayBuffer`, so the page must be cross-origin isolated
(COOP `same-origin` with COEP `require-corp` or `credentialless`).

In user mode the page runs a program from the filesystem with no kernel, the console being its
terminal:

```js
const vm = new Armless({
  wasm: "armless.wasm",
  memory_mb: 512,
  filesystem: { json: "filesystem.json", baseurl: "filesystem/" },
  user: { argv: ["python3"], env: ["PATH=/usr/bin:/bin", "TERM=xterm-256color"], cwd: "/", cols: 80, rows: 24 },
  // or, a snapshot of one: initial_state: "state.bin.zst", user_state: "user.bin"
});
vm.on("stop", ({ reason, code }) => console.log(reason, code)); // "exit", 0
```

## Browser API

### Options

| option | meaning |
|---|---|
| `wasm`, `wasm_smp` | the wasm to load (a URL, bytes or a `WebAssembly.Module`). With `cpus > 1`, `wasm_smp`, or else `wasm` with `armless.wasm` replaced by `armless-smp.wasm` |
| `memory_mb` | the guest's RAM (default 512). A restored snapshot only commits the pages it holds |
| `cpus` | number of CPUs (default 1) |
| `filesystem` | `{ json, baseurl }`: the 9p root's tree and where its blobs are fetched from |
| `fs_cache_mb` | cap on decompressed file contents kept in the page (256 by default); the guest's page cache keeps what it uses often |
| `predecode_mb` | cap on the pre-decoded instructions each CPU keeps (24 by default), held for the life of the machine |
| `initial_state` | a snapshot to restore (URL or bytes, zstd or not) |
| `boot` | for a cold boot: `{ kernel, initrd, cmdline }` (`cmdline` defaults to `console=ttyS0`), or `{ raw, addr }` for a bare-metal binary; without `kernel`, the kernel and initrd under `/boot` in the filesystem |
| `user` | user mode, instead of `boot`: `{ argv, env, cwd, cols, rows }`, a program started from the filesystem as execve would (by `PATH` in `env` when `argv[0]` has no slash; cwd `/` and 80x24 by default). One CPU only |
| `user_state` | user mode from a snapshot: the process (URL or bytes) saved with the machine `initial_state` restores, by `save_process()` or natively by `Process::save` (snowglobe's `system/user.bin`). The machine must be made as the one saved was: `memory_mb`, `filesystem`, and `console`, `network` on or off |
| `network` | turns on virtio-net; see below |
| `console` | virtio-console on or off (default on) |
| `mac` | the NIC's MAC address, `"aa:bb:cc:dd:ee:ff"` |
| `jit`, `jit_threshold` | the wasm JIT on or off (default on) and how often a block runs before it is compiled |
| `autostart` | run as soon as it is set up (default true); otherwise call `run()` |
| `debug` | off by default. On, the machine samples its memory every few seconds and where it moves (loaded, restored, a save), keeps the last 400 in `vm.debug.samples` and emits them as `memory` events; see `memory_report()` |

`network` options:

- `tls`: the CA as `tls.json` (`caSubject`, `caKeyId`, `leafSpki`, `caKey` and `leafKey`,
  base64 DER), as an object or a URL. Without it, port 443 is plain TCP.
- `fetch` and `WebSocket`: replace the page's own.
- `upgrade_https`: ask for `https://` and `wss://` when the guest uses `http://` and
  `ws://`. This is the default on https pages.
- `tunnels`: `[{ host, port, url }]`, guest TCP destinations carried over a WebSocket to
  `url` as binary messages (websockify, websocat, Neon's wsproxy). Leave out `host` to match
  any name on that port. `url` can be a function of `{ host, port }` returning a URL or a
  promise of one, called for each connection (to add a fresh access token, say); if it
  throws or rejects, the guest's connection is refused.

### Events

Subscribe with `vm.on(name, fn)` (`off` removes).

- `console` and `serial`: output bytes.
- `ready`: set up. `progress` (`{ file, loaded, total }`): a snapshot or file loading.
- `blob`: a file being fetched. `stop` (`{ reason }`) and `error`.
- In user mode, `console` is what the program writes to its terminal; `stop` is
  `{ reason: "exit", code }` when it exits, `{ reason: "fault", message }` when it faults; and
  `idle` says it has gone quiet waiting for input (every thread waits, none on a file being
  fetched), once until it prints or gets input again.
- `memory`: a sample of the memory (with the `debug` option).
- `tcp-connection`: a `Connection` from the guest to a port nothing else handles.

A `Connection` has `host` (the name the guest resolved) and `port`, and supports
`accept()`, `refuse()`, `hold()` (decide later), `write()`, `close()` and `drain()`, plus
the `data`, `end`, `connect` and `close` events. A page can't make raw TCP connections, so
connections nobody accepts, and no tunnel takes, are refused.

### Calls

- Input: `console_input(data)`, `console_resize(cols, rows)`, `serial_input(data)`.
- Files: `provide_blob(name, data)` to preload a blob, `has_blob(name)`.
- `connect(port)`: a connection into a server in the guest.
- Snapshots: `save_state()` returns the snapshot as one buffer; `save_state_pieces()`
  returns it in pieces of about 1 MiB, to write or compress as they come;
  `restore_state(src, { user_state })` replaces the machine's state (and in user mode the
  process).
- User mode: `console_input` and `console_resize` go to the program's terminal;
  `close_input()` ends its input (reads past what is left return 0), `kill(sig = 2)` sends it a
  signal; `save_process()` returns the process's state, which goes with `save_state()`'s as
  `user_state` with `initial_state` (save both while stopped); `user_status` says
  `{ state: "running" | "idle" | "exited" | "fault", waiting_for_input, waiting_on_blobs,
  wake_ms, code }`, and `idle` fires when the program waits for input (not when it only sleeps
  or waits for files). An idle program costs nothing: it runs again at its next deadline
  (`wake_ms`), on input, or when a file it waits on arrives.
- `memory_report({ scan })`: what the page's memory holds, split into the pieces armless can tell
  apart: the whole linear memory (which never shrinks), the guest's RAM, the host's heap (live,
  peak, big blocks, freed blocks kept to be reused), the file system (blobs fetched, files the
  guest wrote) and the JIT. With `scan` (the default) it also counts the 4 KiB pages that hold a
  non-zero byte before, in and after the RAM: the memory in use, as opposed to how far it has
  grown. It is what to read when a page is bigger than the guest's RAM explains.
- Lifecycle: `run()`, `stop()` and `destroy()`; `vm.ready` is a promise for the setup.

## Native API

```rust
use armless::{native::{Egress, HostOptions, NativeHost}, runner::{self, Runner}, vm::Config};

let (mut vm, clock) = runner::new_vm(Config { memory_size: 512 << 20, cpus: 2, ..Default::default() })?;
vm.board.load_filesystem(&std::fs::read("filesystem.json")?)?;
vm.board.set_tls(&std::fs::read("tls.json")?)?;  // the guest's HTTPS ends in the machine
let host = NativeHost::spawn(vm.board.clone(),
    HostOptions { blobs: Some("filesystem".into()), network: true, egress: Egress::from_env(), tunnels: vec![] },
    Box::new(|event| { /* console output, files read, ... */ }));
let mut runner = Runner::new(vm, clock, false);
runner.load_state(&std::fs::read("state.bin.zst")?)?;
runner.start();
```

- `Egress::from_env()` goes out through the proxy the environment names (`HTTPS_PROXY`,
  `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`), with TLS checked against the system's roots
  (`SSL_CERT_FILE` included). That covers sandboxes whose only way out is a
  TLS-intercepting proxy. Private and loopback addresses are off limits unless
  `allow_private` is set. [`examples/fetch.rs`](examples/fetch.rs) makes a request this way.
- `Board::set_fs_cache` is the native counterpart of `fs_cache_mb`.
- `runner::new_vm` and every `Runner` restore seed the machine from the host
  (`Board::seed`), so a restored guest gets a new VM generation id and Linux reseeds its
  random generator at once.
- `HostOptions::blob_source` serves blobs from anywhere (an archive in memory, say)
  instead of a directory.

## Building

```sh
cargo build --release           # the crate and armless-run
sh build-wasm.sh                # build/armless.wasm (with user mode)
SMP=1 sh build-wasm.sh          # build/armless-smp.wasm
```

The wasm builds need the `wasm32-unknown-unknown` target, plus clang and lld for the C zstd
decoder in [`lib/zstd`](lib/zstd). `SMP=1` builds std from source with atomics, so it also
needs the `rust-src` component (`rustup component add rust-src`). `js/armless.js` is used
as it is. armless.wasm is the cdylib of the `armless-user` crate, so it has user mode's exports
next to the machine's; armless-smp.wasm is armless's alone (user mode runs on one CPU).

Cargo features: `native-jit` (default; Cranelift), `hypervisor` (see
[docs/hypervisor.md](docs/hypervisor.md)) and `wasm-api` (compiles the wasm export layer
natively, to type-check it; `armless-user` has one of the same name for its own exports).

## How it works

[docs/armless.md](docs/armless.md) has the design in full; the main points:

- **Interpreter and JIT.** The interpreter decodes and runs instructions. Hot entries (1024
  runs by default, `ARMLESS_JIT_THRESHOLD`) are compiled a page at a time: to WebAssembly
  in the browser, to host code with Cranelift natively, on a compiler thread per CPU.
  Compiled code covers the integer instructions, SIMD&FP loads and stores, the common
  integer NEON operations, scalar floating point (inline when FPCR is as Linux runs it and
  the numbers are ordinary), and in-page indirect branches. Vector floating point runs lane
  by lane through the interpreter's arithmetic; atomics and crypto instructions go to the
  interpreter. Anything else is handed to the interpreter one instruction at a time.
- **Native JIT.** `ARMLESS_JIT=native` turns it on for `runner::new_vm` (or call
  `Cpu::enable_native_jit`). It needs no hypervisor, so it works in sandboxes. Each CPU keeps
  up to `ARMLESS_JIT_CODE_MB` (default 16) MB of compiled code and starts over when full.
  `armless-run --fallbacks` or `ARMLESS_JIT_HELPER_STATS=1` counts what falls back to the
  interpreter, by opcode.
- **Warm JIT.** A restore starts with no compiled code. `Runner::jit_profile` lists the pages
  a machine compiled and ran, `jit::profile::merge` combines the profiles of many runs, and
  `Runner::jit_prewarm` compiles a profile's pages as soon as a machine restored from the
  same snapshot starts. A page that holds other code than when it was profiled is skipped.
- **Hardware backends.** `ARMLESS_HV=1` with the `hypervisor` feature runs the CPUs on the
  host's own cores where Hypervisor.framework (Apple silicon) or `/dev/kvm` (Linux arm64) is
  available, and falls back to the interpreter and JIT otherwise. See
  [docs/hypervisor.md](docs/hypervisor.md), including the macOS entitlement.
- **MMU.** A software page-table walker with a direct-mapped TLB, ASID-aware; TLBI by VA
  invalidates what it names and other invalidations flush everything. With several CPUs,
  TLB shootdowns are delivered between them. Code pages written by the guest or by DMA are
  invalidated on every CPU.
- **SMP.** One host thread or Web Worker per CPU. Devices run on whichever thread touches
  them, under one lock. Atomics are host compare-and-swap on guest RAM. In the browser,
  `armless-smp.wasm` imports a shared memory that every worker instantiates on.
- **Devices.** UART, GIC and RTC are memory-mapped; the virtio devices sit on a PCI bus in
  fixed slots (1 console, 2 9p with tag `host9p`, 3 network). The machine never calls the
  host synchronously: what the host must do is queued as events and answered through
  `Board`.
- **Snapshots.** CPUs, GIC, timers, device state and the filesystem tree (with the files the
  guest wrote), plus every non-zero RAM page, in a streamable format. Blobs are not saved;
  the guest's unmodified files are fetched again after a restore. A machine can be saved as
  one of its first n CPUs once the guest has taken the others offline.
- **Browser memory model.** WebAssembly memory never shrinks, so every peak stays committed
  until the page goes. The decompressed file cache is capped (`fs_cache_mb`), files the guest
  writes grow a chunk at a time, a restore only commits the pages the snapshot holds, and a
  save is produced a piece at a time rather than as one buffer the size of the snapshot.
  With a large guest, write or compress the pieces as they arrive instead of calling
  `save_state()`, which joins them into one buffer.

## Filesystem images

Test and example filesystems are built with scripts under [`tests/fs`](tests/fs):
`make-9pfs.sh` builds a test image, `alpine/` a real Alpine, and `fs2json.py` with
`copy-to-sha256.py` turn a directory into the tree and its content-addressed blobs. Files
of 2 MiB and more are split into chunks (`--chunk-threshold`); `--no-chunks` makes images
for armless 0.2.10 and older.

## Testing

| | |
|---|---|
| `cargo test --release --workspace` | unit tests (user mode's too); the bare-metal CPU suite (`guest/`); four CPUs racing on shared counters with LSE atomics and exclusives; TLS and HTTP of the web relay against rustls |
| `tests/difftest/run.sh CLASS N SEED` | random instructions against qemu-aarch64; CLASS is `int`, `fp`, `simd`, `simds`, `ldst`, `crypto` or `lse` (`DUT=jit`: the wasm JIT) |
| `tests/linux/{boot,userspace,smp}.sh` | Linux on `armless-run`; userspace and SMP stress output against qemu-aarch64 |
| `ARMLESS_9PFS=DIR cargo test --release --test vm` | the snowglobe flow natively: 9p root, console, snapshot, restore |
| `node tests/js/flow.mjs DIR [CPUS]` | the same through `armless.js`, plus HTTP over fetch and port forwarding |
| `node tests/js/boot.mjs`, `run-bare.mjs`, `alpine.mjs` | kernels, bare-metal binaries and a real Alpine through `armless.js` |
| `node tests/js/user.mjs SITE` | user mode through `armless.js`: a `--kernel none` snowglobe site restored from its native snapshot, asked a question, saved and restored again; `--exec FSDIR PROGRAM ARGS` starts a program from a filesystem |
| `elixir bench/run.exs` | the benchmark guest under the interpreter, the native JIT and the wasm JIT (node) |
| `elixir bench/sites.exs ../snowglobe` | real workloads: built snowglobe sites running their own commands |
| `cargo bench --bench machine` | host-side costs with criterion: an instruction interpreted and compiled, a page's compile, a snapshot |

[docs/profiling.md](docs/profiling.md) explains the benchmarks and profiles. CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs the unit tests (with and
without overflow checks), clippy natively and for wasm32, the KVM backend's lints, the wasm
builds, and a Linux job that boots a kernel and compares userspace and SMP output with
qemu-aarch64, natively, with the native JIT and through `armless.js`.

## Releases

A push to `main` that changes the version in `Cargo.toml` publishes a GitHub release
`armless-v<version>` (once per version). It carries `armless.js`, `armless.wasm`,
`armless-smp.wasm`, and `armless-web.tar.gz` with those three, the license and
`docs/armless.md`. Native programs use the crate itself, as a git dependency on the tag
(`publish = false`: it is not on crates.io). User mode is the `armless-user` crate in the same
repository, a git dependency on the same tag.

## License

BSD 2-Clause, see [LICENSE](LICENSE).
