# armless design

## The machine

```text
0x0000_0000 .. memory_size   RAM (kernel Image at 0x0020_0000; DTB and initrd at the top)
0xE000_0000 .. 0xEFFF_FFFF   PCI MMIO window (virtio BARs, 0x10000 per slot)
0xF000_0000                  GICv2 distributor     0xF001_0000  GICv2 CPU interface
0xF010_0000                  16550 UART (ttyS0)    0xF011_0000  PL031 RTC
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
so `TcpOpen` carries the name the guest resolved:

- **Browser:** port 80 goes to `fetch()` (`Armless.http_over_fetch`), upgraded to
  https:// on https pages. A page can claim other ports, for example snowglobe's
  TLS bridge on 443.
- **Native:** `NativeHost` connects real sockets, or refuses everything when the
  network is off.

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
      open 9p fids, waiting requests, network names)
RAM:  (u32 page, 4096 bytes) for every non-zero page, then u32 0xffffffff
```

`StateLoader` streams this, zstd-compressed or not, straight into guest memory.
Blobs are not saved; the guest's unmodified files are fetched again after a restore.

## SMP in the browser

`armless-smp.wasm` is built with atomics and imports a shared memory. The page's
instance creates the machine and serves host calls. Each vCPU worker instantiates
the same module on that memory, with its own stack, TLS and JIT function table, and
calls `armless_run` in a loop. The workers raise `host_notify` when events are
waiting. A shared control block parks every worker for `save_state`.
