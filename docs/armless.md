# armless design

## The machine

```text
0x0000_0000 .. memory_size   RAM (kernel Image at 0x0020_0000; DTB and initrd at the top)
0xE000_0000 .. 0xEFFF_FFFF   PCI MMIO window (virtio BARs, 0x10000 per slot)
0xF000_0000                  GICv2 distributor     0xF001_0000  GICv2 CPU interface
0xF010_0000                  16550 UART (ttyS0)    0xF011_0000  PL031 RTC
0xF012_0000                  VM generation ID (16 bytes, SPI 40, edge)
0xF100_0000 .. +0x10_0000    PCI ECAM, bus 0
```

Linux boots the arm64 way: the kernel is at a 2 MiB-aligned base, x0 holds the
device tree, the MMU is off, and everything runs at EL1h. Kernels can be raw `Image`,
gzip, zstd or EFI zboot (`kernel.rs`). They come from the host, or from `/boot` in
the 9p filesystem (`Vm::boot_from_fs`).

PCI slots are fixed: 1 = console, 2 = 9p (tag `host9p`), 3 = network. Each device
is a modern virtio-pci function with one memory BAR (common config, ISR, device
config, notifications) and a legacy INTx line. The device tree marks the bus
`linux,pci-probe-only`, so Linux keeps the BARs armless assigned.

## Code map

| | |
|---|---|
| `cpu.rs`, `a64.rs`, `advsimd.rs`, `crypto.rs`, `fp.rs`, `simd*.rs`, `mmu.rs`, `sysreg.rs` | the CPU |
| `jit/` | page-at-a-time translation to WebAssembly modules (wasm builds only) |
| `gic.rs`, `rtc.rs`, `smp.rs` | shared machine state, multi-CPU GIC, PSCI, TLB shootdowns |
| `dev/` | UART, PCI, virtio transport, console, 9p + filesystem, net + network stack |
| `vm.rs` | `Board` (devices + host API), `Vm` (with CPUs), snapshots |
| `runner.rs`, `native.rs`, `platform.rs` | native hosting: threads, blob directory, TCP NAT |
| `wasm.rs`, `js/armless.js` | the browser build |
| `../user/` | user mode: a static Linux executable with no guest kernel (`armless-user`, `armless-exec`) |

## Host interface

The machine never calls the host synchronously for I/O. What the host has to do
is queued as `Event`s, and the host answers through `Board`:

| event | answer |
|---|---|
| `Serial(bytes)`, `Console(bytes)` | (display it) |
| `Blob(name)` | `provide_blob(name, contents)` or `blob_failed(name)` |
| `HttpRequest { id, method, url, headers, body }` | `http_respond` (status, headers), `http_body`…, `http_end`; or `http_fail` |
| `WsOpen { id, url, protocols }` | `ws_accept(id, protocol)` or `http_fail`; then `ws_send`/`ws_close` |
| `WsMessage`, `WsClose` | (deliver to the other end) |
| `TcpOpen { id, host, port }` | `tcp_accept(id)` or `tcp_refuse(id)`, then `tcp_send`/`tcp_close` |
| `TcpData`, `TcpEof`, `TcpClosed` | (deliver to the other end) |
| `TcpConnected { id }` | after `tcp_connect(port)`, the host's connection into the guest is up |

Input goes through `console_input`, `console_resize` and `serial_input`. Console
input is queued until the guest posts receive buffers, so nothing is dropped.

A 9p request that needs a blob that hasn't arrived waits (its descriptor chain is
held) and is answered when the blobs it needs have all arrived. Fetched blobs are
cached up to 256 MiB, oldest evicted first, except the chunks a waiting read covers.

## The filesystem

`filesystem.json` is v86's format 3: `fsroot` lists `[name, size, mtime, mode, uid,
gid, target]`, where `target` is a directory's children, a symlink's target, or a
regular file's blob. A file can instead be in chunks, `[[blob, length], ...]`, whose
lengths add up to `size`:

```json
["big.bin", 8388608, 0, 33188, 0, 0, [["0e2c9a7d41.bin.zst", 878612], ["...", 7509996]]]
```

A read fetches only the chunks it covers, and a write copies only the chunks it
touches into memory (truncating keeps a prefix of a chunk without fetching it).
Chunks are plain blobs to the host, so identical chunks are shared across files and
images. `tests/fs/cdc.py` cuts them where the contents say (a gear hash, 256 KiB to
4 MiB, about 1 MiB), so an insertion only changes the chunks around it.

## The network

The guest is 192.168.86.100/24, and the router 192.168.86.1 answers ARP, DHCP, DNS
and ping. DNS gives every name an address of its own (10.x.y.z, kept in snapshots),
so the machine always knows which *name* a connection is for.

### The web relay (`dev/web`)

Browsers and most sandboxes only let HTTP out: `fetch()` and WebSocket, or an HTTP(S)
proxy. So the guest's connections to port 80, and to 443 once `Board::set_tls` gave
the site's CA, end inside the machine:

- **TLS 1.3** on 443 (`tls.rs`) uses TLS_AES_128_GCM_SHA256 with X25519 or P-256 and
  an ECDSA P-256 certificate minted per SNI name, signed by the site's CA (`cert.rs`).
  The guest trusts that CA. There is no HelloRetryRequest. Every current OpenSSL,
  Node, Python, Go, Rust and Java client offers what it needs.
- **Resumption:** every handshake ends with a session ticket (valid a day, for that SNI
  name). A client that comes back with it skips the certificate and its signature, but
  still makes a fresh X25519 or P-256 exchange (psk_dhe_ke only; no 0-RTT). Tickets are
  sealed with a key made from the host's seed and forgotten at every `Board::seed`, so
  copies of one snapshot can't use each other's tickets; a ticket the machine can't
  open, or one that is too old or for another name, just means a full handshake. A
  wrong binder fails it.
- **HTTP/1.1** inside (`mod.rs`) keeps connections open: Content-Length or chunked
  bodies, `Expect: 100-continue`, and requests answered one at a time (a pipelined one
  waits in the buffer, and is a new `HttpRequest` once the response before it ends).
  Responses are streamed with the host's Content-Length, or chunked when it gave none
  (fetch() never does); a host that sends less, or more, than it said closes the
  connection. HTTP/1.0 and `connection: close` requests get a response that ends with
  the connection, as does a guest that has finished sending, after its last request.
- **WebSocket upgrades** become `WsOpen` and the frames become messages. Pings are
  answered in the machine.

The host only sees `HttpRequest`/`WsOpen`, with full https://, wss://… URLs:

- **Browser:** `fetch()` and `WebSocket`, upgraded to https:// and wss:// on https
  pages (CORS applies).
- **Native:** `native::Egress`, a small HTTP/1.1 + WebSocket client on rustls. It
  goes direct or through the environment's proxy (CONNECT for TLS, absolute-form for
  plain HTTP, `NO_PROXY` honoured) and checks TLS against the system roots, so
  TLS-intercepting sandbox proxies work. Private addresses are refused unless
  allowed.

Randomness for TLS comes from the host (`Board::seed`): the browser uses
`crypto.getRandomValues`, natively it comes from `/dev/urandom`. It is seeded again
after every restore.

The same seed gives the guest a new VM generation ID (`microsoft,vmgenid` in the device
tree) and raises its interrupt: Linux reseeds its random generator at once
(`add_vmfork_randomness`). Without it, every restore of a snapshot would draw the same
"random" numbers (UUIDs, keys, nonces) as the machine that was saved.

### Other TCP

Other ports reach the host as `TcpOpen` streams:

- **Native:** a real connection, direct or tunnelled with CONNECT through the proxy.
- **Browser:** refused unless the page takes it.

Either host can carry chosen destinations over a WebSocket instead: a **tunnel** maps a
name and port (or any name, on a port) to a ws:// or wss:// URL, and each connection to
it becomes one WebSocket whose binary messages are the stream's bytes (what websockify,
websocat and Neon's wsproxy speak). It is `HostOptions::tunnels` natively (`Tunnel::parse`
reads `[HOST:]PORT=URL`) and `network.tunnels` in the browser, where it is the only way
out for raw TCP, and where the URL can be made per connection by a function (a fresh
access token for a gateway that holds the real credentials, so none is in the page). The guest's SYN waits until the WebSocket is open, so a tunnel that
can't be reached is refused like a closed port. A WebSocket can't half-close: the guest
finishing its side closes it.

With the network off, everything is refused.

TCP towards the guest respects its window. The link never drops frames: they wait
for receive buffers. A retransmit timer (`Board::tick`) covers what the guest's own
stack drops. Connections are not part of snapshots: after a restore, the guest gets
RST for any it still has.

## Threads

Each CPU runs in its own host thread or Web Worker. Devices run on whichever thread
touches them, under one lock: a vCPU doing MMIO, or the host delivering input. Idle
CPUs sleep on a wake counter (`Atomics.wait` in workers) until their timer expires
or something kicks them. Device DMA into a page holding translated code invalidates
that code on every CPU (`Machine::code_pages`).

With several CPUs, a store-exclusive succeeds if memory still holds what its load-exclusive
read (a host compare-and-swap, as QEMU does), and the LSE atomics (CAS, SWP, LDADD...) are host
compare-and-swap loops on guest RAM, so both are atomic against every other CPU. 128-bit pairs
(STXP, CASP of X registers) are updated under one machine-wide lock. Unaligned atomics take an
alignment fault; the JIT leaves atomics and the crypto instructions to the interpreter.

For a snapshot every CPU is paused between instruction slices. A paused CPU counts
as idle, so a running CPU waiting for TLB acknowledgements does not wait for it.

## Snapshots (format 3)

```text
"ARMLESS\x03"  u64 RAM size  u32 CPUs  u64 meta length
meta: CPUs, GIC, timers, RTC (state.rs), then the devices (UART, PCI functions,
      virtio queues, console, the filesystem tree with files the guest wrote
      (of a chunked file, only the chunks it wrote),
      open 9p fids, waiting requests, network names), then tagged extensions
      (u32 tag + data; "P9OW": who each 9p fid belongs to, "VGID": the VM generation ID)
RAM:  (u32 page, 4096 bytes) for every non-zero page, then u32 0xffffffff
```

`StateLoader` streams this, zstd-compressed or not, straight into guest memory.
Blobs are not saved; the guest's unmodified files are fetched again after a restore.
Chunked files are a new content kind in the tree, so older armless can't load a snapshot
that has one; snapshots without them are unchanged.
Extensions go after the devices, so snapshots without them still load (9p fids then
belong to root, as they used to).

A machine can be saved as one of its first n CPUs (`save_state_cpus`) once the guest has taken
the others offline (`echo 0 > /sys/devices/system/cpu/cpuN/online`, which ends in PSCI CPU_OFF):
their state is left out and shared interrupts aimed only at them go to CPU 0. A machine with n
CPUs restores it; the guest sees the rest as offline CPUs, and CPU_ON for them fails. snowglobe
boots and warms a site on more CPUs than it ships with this way.

## SMP in the browser

`armless-smp.wasm` is built with atomics and imports a shared memory. The page's
instance creates the machine and serves host calls. Each vCPU worker instantiates
the same module on that memory, with its own stack, TLS and JIT function table, and
calls `armless_run` in a loop. The workers raise `host_notify` when events are
waiting. A shared control block parks every worker for `save_state`.

## User mode (experimental)

`user/` (the `armless-user` crate, and `armless-exec` on the command line) runs a Linux program
with no guest kernel. The machine is the same; what is missing is EL1. The CPU runs the program
at EL0 with the MMU off and no vector table, so every exception it takes stops the CPU
(`StopReason::UnhandledException`, cpu.rs) with ESR, ELR and FAR. That is true in the
interpreter and in both JITs, so user mode needs nothing from the CPU it didn't already do. The
host reads the syscall from x8 and x0–x5, answers it in x0, and resumes at ELR.

- **Programs:** static or dynamically linked. A dynamic one's loader (PT_INTERP, musl's
  `ld-musl-aarch64.so.1` say) comes from the machine's filesystem, as execve has it, and maps the
  libraries itself; `Process::exec` finds the program by PATH.
- **Files:** the machine's filesystem, the one the 9p device serves a Linux guest
  (`Board::with_fs`): open, read and write (by offset too), stat and statx, getdents64,
  readlink, getcwd and chdir, mkdir, unlink, rename, symlink, link, truncate; paths resolve
  through symlinks and `..`. File contents still arrive as blobs, when first read: a read that
  needs one parks its thread, the machine asks the host for it (`Board::want_blobs`), and the
  call runs again once it is there, as a 9p request does. Descriptors are Linux's: open file
  descriptions with their offset, shared by dup. `/dev/null`, `zero`, `urandom` and `tty`, and a
  few `/proc` files, are made here. File mappings (`mmap` of a file) are private copies.
- **The terminal:** stdin, stdout and stderr are a terminal with termios (TCGETS, TCSETS), a
  window size (`Process::resize`, TIOCGWINSZ) and a line discipline, as n_tty: in canonical
  mode a line is edited and echoed before it is read; in raw mode bytes pass through; Ctrl-C
  is SIGINT; output newlines start a line (ONLCR).
- **Pipes, eventfd, epoll** (edge-triggered and one-shot too), **poll and select.**
- **Memory:** VA is PA. The ELF's segments go where they ask (a PIE, and the loader, at a base
  of their own), `brk` after them, the stack at the top of RAM and `mmap` below it, from a free
  list; `mremap` moves or grows. Freed ranges are given back to the host with `Ram::discard`
  (fresh zero pages), so a program that reserves and releases large ranges (Go reserves about
  600 MB of address space at start) costs only what it touches. RAM must still be large
  enough for those reservations.
- **Threads:** every guest thread is a saved register context, and they take turns on one
  CPU in slices of instructions; a syscall answered on the way counts against the slice.
  `clone` (threads only), futexes (with timeouts and bitsets), `set_tid_address` and
  `CLONE_CHILD_CLEARTID`.
- **Blocking:** a call that can't complete (a read of the terminal or a pipe, a poll, a wait
  for blobs) parks its thread with its pc on the svc, and runs again when something changes,
  or returns at its deadline. So whatever a program waits on, a snapshot can be taken.
- **Clocks:** `clock_gettime`, `nanosleep`, `clock_nanosleep`. The monotonic clock goes on
  across a snapshot and its restores.
- **Signals:** handlers are recorded and not called yet; a signal with no handler whose
  default is to end the process ends it.
- **Snapshots:** the machine's own snapshot holds RAM and the filesystem; `Process::save` holds
  the rest (threads, their waits, the memory map, descriptors and pipes, the terminal, the
  clock), a few KB. `Process::restore` takes it on a machine that loaded the snapshot.

Not yet: page tables (so no PROT_NONE, no nil-pointer faults, no `fork`), processes (`fork`,
`execve`, `wait4`), sockets, signal delivery, more than one vCPU, and the hypervisor backends
(an SVC at EL0 traps to EL1 in hardware there, not to the host).

What runs: snowglobe's `go` (Bubble Tea, static), `python` (the 3.14 REPL, 122 shared
libraries) and `duckdb` (C++) examples, from their own images' filesystems; a Python REPL saved
with its state and restored in 120 ms. `user/corpus/survey.exs` traces what each of snowglobe's
examples asks of a kernel, for what comes next (processes, then sockets).

Measured as a snowglobe site (a Go program with eight goroutines, macOS, the same
`snowglobe-vm` for each): the snapshot is 2.5 MB raw and 0.6 MB compressed, against 22.1 and
3.6 MB under a stripped Linux and 50.6 and 10.2 under Alpine's `linux-virt`; a machine's
footprint is 22 MB, against 57 and 136.
