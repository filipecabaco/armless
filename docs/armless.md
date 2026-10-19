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
| `cpu.rs`, `a64.rs`, `advsimd.rs`, `fp.rs`, `simd*.rs`, `mmu.rs`, `sysreg.rs` | the CPU |
| `jit/` | page-at-a-time translation to WebAssembly modules (wasm builds only) |
| `gic.rs`, `rtc.rs`, `smp.rs` | shared machine state, multi-CPU GIC, PSCI, TLB shootdowns |
| `dev/` | UART, PCI, virtio transport, console, 9p + filesystem, net + network stack |
| `vm.rs` | `Board` (devices + host API), `Vm` (with CPUs), snapshots |
| `runner.rs`, `native.rs`, `platform.rs` | native hosting: threads, blob directory, TCP NAT |
| `wasm.rs`, `js/armless.js` | the browser build |

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
held) and is answered when the blob arrives. Fetched blobs are cached up to
256 MiB, oldest evicted first.

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
  The guest trusts that CA. There is no resumption and no HelloRetryRequest. Every
  current OpenSSL, Node, Python, Go, Rust and Java client offers what it needs.
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

For a snapshot every CPU is paused between instruction slices. A paused CPU counts
as idle, so a running CPU waiting for TLB acknowledgements does not wait for it.

## Snapshots (format 3)

```text
"ARMLESS\x03"  u64 RAM size  u32 CPUs  u64 meta length
meta: CPUs, GIC, timers, RTC (state.rs), then the devices (UART, PCI functions,
      virtio queues, console, the filesystem tree with files the guest wrote,
      open 9p fids, waiting requests, network names), then tagged extensions
      (u32 tag + data; "P9OW": who each 9p fid belongs to, "VGID": the VM generation ID)
RAM:  (u32 page, 4096 bytes) for every non-zero page, then u32 0xffffffff
```

`StateLoader` streams this, zstd-compressed or not, straight into guest memory.
Blobs are not saved; the guest's unmodified files are fetched again after a restore.
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
