# armless

An AArch64 virtual machine that runs unmodified arm64 Linux in a browser tab, and
natively. It is written in Rust:

- CPU: ARMv8-A EL0/EL1 with FP/AdvSIMD. Interpreted, with a JIT that translates
  hot pages to WebAssembly.
- SMP: up to 8 CPUs, one Web Worker (or native thread) each.
- Interrupts and time: GICv2 and the generic timer.
- Devices, all in Rust:
  - a PL031 RTC and a 16550 UART (`ttyS0`);
  - virtio-console (`hvc0`, with the terminal size);
  - a 9p root filesystem built from content-addressed blobs that are fetched on
    first read;
  - virtio-net with a user-mode network. DHCP, DNS and ping are answered
    locally.
  - a web relay inside the machine. HTTP and HTTPS (ports 80 and 443; HTTPS is
    terminated with a CA the guest trusts) and WebSockets reach the host as
    requests to make, so a page answers them with `fetch()` and a native host with
    any HTTP client, proxies included. Other TCP connections become streams.
- Snapshots of the whole machine, restored by streaming.

The host (a page, or a native program) only provides time, file contents and the
other end of TCP connections. [snowglobe](https://github.com/filipecabaco/snowglobe)
uses armless to turn Docker images into static sites that restore a running app
in about a second.

## In a page

```html
<script src="armless.js"></script>
<script>
  const vm = new Armless({
    wasm: "armless.wasm",            // armless-smp.wasm next to it for cpus > 1
    memory_mb: 512,
    cpus: 1,
    filesystem: { json: "filesystem.json", baseurl: "filesystem/" },
    initial_state: "state.bin.zst",  // or boot: { kernel, initrd, cmdline }
    network: { tls: "tls.json" },    // the guest's HTTP(S) and WebSockets go out with
  });                                //   fetch() and WebSocket
  vm.on("console", (bytes) => term.write(bytes));
  term.onData((text) => vm.console_input(text));
  vm.on("ready", () => vm.console_resize(term.cols, term.rows));
</script>
```

Several CPUs need `SharedArrayBuffer`, so the page must be cross-origin isolated
(COOP `same-origin` + COEP `require-corp` or `credentialless`).

Events and calls:

- **Events:** `console` and `serial` (bytes), `progress` (`{file, loaded, total}`),
  `blob` (a file being fetched), `tcp-connection` (a `Connection`), `stop`
  (`{reason}`) and `error`.
- **Memory:** `memory_mb` is the guest's RAM; a restored snapshot only commits the pages it
  holds. `fs_cache_mb` caps the decompressed file contents kept on the host (256 by default;
  the guest's page cache keeps what it uses often), `Board::set_fs_cache` natively.
- **Calls:** `console_input`, `console_resize`, `serial_input`, `provide_blob`
  (preload), `connect(port)` (a connection into the guest), `save_state()`,
  `restore_state()`, `stop()`, `run()` and `destroy()`.

`network` options:
- `tls`: the CA as `tls.json` (`caSubject`, `caKeyId`, `leafSpki`, `caKey` and
  `leafKey`, base64 DER). Without it, port 443 is plain TCP.
- `fetch` and `WebSocket`: replace the page's own.
- `upgrade_https`: ask for https:// and wss:// when the guest uses http:// and ws://.
  This is the default on https pages.
- `tunnels`: `[{ host, port, url }]`, guest TCP destinations carried over a WebSocket
  to `url` as binary messages (websockify, websocat, Neon's wsproxy). Leave out `host`
  to match any name on that port. `url` can be a function of `{ host, port }`, returning a
  URL or a promise of one, called for each connection (to add a fresh access token, say);
  if it throws or rejects, the guest's connection is refused.

TCP connections to other ports are `tcp-connection` events with a `Connection`. It
has `host` (the name the guest resolved) and `port`, and supports `accept()`,
`refuse()`, `hold()` (decide later), `write()`, `close()` and `drain()`, plus the
`data`, `end`, `connect` and `close` events. A page can't make raw TCP connections,
so connections nobody accepts (and no tunnel takes) are refused.

## Natively

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

`Egress::from_env()` goes out through the proxy the environment names
(`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`), with TLS checked against the
system's roots (`SSL_CERT_FILE` included). That covers sandboxes whose only way out is
a TLS-intercepting proxy. Private and loopback addresses are off limits unless
`allow_private` is set.

`armless-run` is a command-line runner for kernels and bare-metal binaries
(`cargo run --release --bin armless-run -- --help`).

## Building

```sh
cargo test --release            # unit tests, the bare-metal CPU suite
sh build-wasm.sh                # build/armless.wasm
SMP=1 sh build-wasm.sh          # build/armless-smp.wasm (needs the rust-src component)
```

The wasm builds need clang and lld (for the C zstd decoder in `lib/zstd`).

## Testing

| | |
|---|---|
| `cargo test --release` | unit tests; bare-metal CPU suite (`guest/`); TLS and HTTP of the web relay against rustls |
| `tests/difftest/run.sh CLASS N SEED` | random instructions vs qemu-aarch64 (`DUT=jit`: the wasm JIT) |
| `tests/linux/{boot,userspace,smp}.sh` | Linux on `armless-run`; userspace and SMP stress output vs qemu-aarch64 |
| `ARMLESS_9PFS=DIR cargo test --release --test vm` | the snowglobe flow natively: 9p root, console, snapshot, restore |
| `node tests/js/flow.mjs DIR [CPUS]` | the same through `armless.js`, plus HTTP over fetch and port forwarding |
| `node tests/js/boot.mjs`, `run-bare.mjs`, `alpine.mjs` | kernels, bare-metal binaries and a real Alpine through `armless.js` |

`tests/fs/make-9pfs.sh` builds the test filesystem, and `tests/fs/alpine/` builds
a real Alpine image. See `docs/armless.md` for the design.

armless started as a fork of [v86](https://github.com/copy/v86). The v86-based
version is kept on the `v86-fork` branch.
