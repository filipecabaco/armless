# armless: an ARM (AArch64) machine for v86

armless is a fork of [v86](https://github.com/copy/v86) that runs an **AArch64**
Linux guest instead of x86, while keeping v86's JavaScript runtime and API. The
goal is for [snowglobe](https://github.com/filipecabaco/snowglobe) to build
`arm64` Docker images into static sites with only a backend switch.

## What snowglobe needs from v86 (the compatibility contract)

From `internal/site/index.html.tmpl`, `internal/tools/build-state.mjs` and
`run.mjs`, snowglobe depends on:

| v86 feature | Used for | Where it lives in v86 |
|---|---|---|
| `new V86({ wasm_path, memory_size, … })` | constructing the machine | `src/browser/starter.js` |
| `filesystem: { basefs, baseurl }`, `fs9p.storage` cache | root filesystem over 9p | `lib/9p.js`, `lib/filesystem.js` |
| `bzimage_initrd_from_filesystem`, `cmdline` | finding the kernel in the 9p fs | `starter.js` |
| `virtio_console`, `virtio_console_adapter`, `virtio-console0-*` bus events | the app console (hvc0) shown in xterm.js | `src/virtio_console.js` |
| `net_device: { type: "virtio", relay_url: "fetch" }`, `network_adapter` | guest networking via `fetch()` | `src/virtio_net.js`, `src/browser/fetch_network.js` |
| `serial0_send`, `serial0-output-byte` | build-time scripting | `src/uart.js` |
| `save_state()`, `initial_state` | snapshot after boot, instant start | `src/state.js`, `cpu.get_state` |
| `emulator-loaded/started`, `download-progress`, `destroy()` | lifecycle | `starter.js` |
| internals: `emulator.v86.cpu.devices.virtio_console.virtio.queues[0]` | lossless console input | `src/virtio.js` |

All of these are **JavaScript**. Only the CPU (`src/rust`) and the x86 platform
glue (BIOS, PIC/APIC, port I/O, x86 Linux boot protocol) are x86-specific. So
armless keeps the JavaScript device layer and replaces what sits underneath.

## Architecture

```
            v86 JavaScript (kept)                         armless (new)
 ┌───────────────────────────────────────┐      ┌──────────────────────────────┐
 │ starter.js  V86 API, events, state    │      │ arm/  Rust → wasm            │
 │ 9p.js  virtio.js  virtio_console.js   │      │  A64 interpreter + wasm JIT  │
 │ virtio_net.js  fetch_network.js       │◄────►│  MMU, exceptions, EL0/EL1    │
 │ uart.js  pci.js  io.js                │ MMIO │  GICv2, generic timer, PL031 │
 └───────────────────────────────────────┘ IRQs │  PSCI, DTB, Image loader     │
                                                 └──────────────────────────────┘
```

### Why the JS devices keep working unchanged

* **RAM at physical address 0.** v86 devices DMA through `cpu.mem8` /
  `cpu.read32s(addr)` using guest-physical addresses. The armless board puts RAM
  at 0 (arm64 Linux does not need RAM at any particular address), so those
  addresses are identical.
* **PCI via ECAM.** On x86, `pci.js` is reached through ports 0xCF8/0xCFC. On
  armless, Linux's `pci-host-ecam-generic` driver accesses config space through
  the ECAM window; JS translates an ECAM offset (bus/device/function/register)
  into the same `pci_write*` / response path.
* **I/O ports via a PIO window.** virtio devices in v86 expose **I/O BARs**
  (`use_mmio: false`). The device tree maps PCI I/O space to a 64 KiB MMIO
  window; an access at `PCI_PIO_BASE + N` is forwarded to `io.port_read*(N)`,
  so `io.js`/`virtio.js` see exactly the port accesses they expect. The
  `linux,pci-probe-only` property keeps v86's BAR assignments.
* **Serial.** The UART is declared as an `ns16550a` at `UART_BASE`; accesses
  are forwarded to ports `0x3F8 + offset`, i.e. v86's `uart.js`, so
  `serial0_send` and `serial0-output-byte` keep working.
* **Interrupts.** `pci.raise_irq` / `lower_irq` call into the CPU; in ARM mode
  they map (slot, pin) to a GIC SPI with the same swizzle the device tree's
  `interrupt-map` describes (`armless_pci_intx_intid`). The CPU only latches the
  line (`armless_set_irq`), so JS never re-enters the core during a callback.

### Board memory map

| Range | Device |
|---|---|
| `0x0000_0000 … memory_size` | RAM (kernel `Image` at 2 MiB, DTB + initrd at the top) |
| `0xE000_0000 … 0xEFFF_FFFF` | PCI 32-bit MMIO window |
| `0xF000_0000` | GICv2 distributor |
| `0xF001_0000` | GICv2 CPU interface |
| `0xF010_0000` | 16550 UART → port 0x3F8 (`uart.js`) |
| `0xF011_0000` | PL031 RTC (wall clock, needed for TLS) |
| `0xF020_0000` | PCI I/O window → ports 0x0000–0xFFFF (`io.js`) |
| `0xF100_0000` | PCI ECAM, bus 0 (`pci.js`) |

Interrupts: virtual timer PPI 27, UART SPI 33, RTC SPI 34, PCI INTA–D SPI 35–38.

### Boot

No BIOS. `boot_linux` follows the arm64 boot protocol: `Image` at
`2 MiB + text_offset`, generated DTB (RAM, CPU, PSCI, GIC, timer, UART, RTC,
PCIe host with `interrupt-map`), initrd, `x0 = DTB`, EL1h, MMU off. Snowglobe's
`bios`/`vga_bios` options are accepted and ignored in ARM mode.

`bzimage_initrd_from_filesystem` keeps working: starter.js looks for the kernel
in `/boot` of the 9p filesystem. ARM kernels are found as `vmlinuz-*` and
recognised by the `ARM\x64` magic; Alpine's arm64 `vmlinuz-virt` is gzip
compressed, so JS decompresses it (`DecompressionStream("gzip")`) before
calling `armless_boot_linux`.

### State snapshots

`save_state` keeps v86's format: RAM is packed by the existing JS code straight
out of wasm memory, device state comes from each JS device's `get_state`, and
the CPU contributes one opaque blob (`armless_save_state`: registers, system
registers, GIC, RTC, timer counter). Restoring continues the guest counter from
the saved value, so timers neither jump nor stall after a snapshot restore.

### JIT

Hot code is translated to WebAssembly, the same way v86 does it for x86: one
module per physical code page (`arm/src/jit`).

- **Entries.** The run loop counts executions of block *entries* (targets of
  taken branches, exceptions and returns). An entry that runs
  `jit_threshold` times (default 1024) gets its page compiled. Other entries
  of the page that got warm (a quarter of the threshold) are compiled with it.
- **Modules.** Each entry gets the basic block starting there (up to 96
  instructions, never past the page). The module's function dispatches on the
  PC with a `br_table`. A direct branch to another compiled entry of the same
  page loops back to that dispatch instead of returning, for up to 1024
  instructions or until interrupt state changes.
- **What is translated.**
  - Integer data processing, including divide, `UMULH`/`SMULH`, `EXTR`,
    `CLZ` and `REV`.
  - Branches, and conditional select/compare.
  - Loads/stores, `LDAR`/`STLR`, and the single-register exclusives. Loads
    and stores probe the interpreter's fast TLB inline.
  - Barriers (no-ops on one core) and `MRS`/`MSR` of the thread and stack
    registers.

  Everything else (FP/SIMD, other system instructions) calls back into the
  interpreter for that one instruction. A system instruction only ends the
  block if it changed something translated code depends on: translation
  regime, EL/SPSel, or unmasking interrupts. Guest state stays in the CPU
  struct in linear memory, so it is exact at every instruction boundary and
  exceptions need no recovery.
- **Compiling.** The module is emitted as wasm bytes. JavaScript compiles it
  asynchronously and installs it in armless.wasm's function table. Until then
  the interpreter keeps running. When more entries of a page get hot, the
  page is recompiled and the new module replaces the old one when ready.
- **Self-modifying code.** Pages holding code are kept out of the fast write
  TLB, so every store to them takes the slow path, which drops the page's
  modules. Device DMA does the same through `armless_jit_dirty`.
- **Options.** `disable_jit: true` turns it off. `jit_threshold: N` changes
  the threshold.

On the glibc/busybox userspace test (about 235M guest instructions, through
the V86 API in Node), the JIT takes the run from 6.3 s to 4.0 s; about 80% of
instructions execute in translated code. The JIT difftest (`DUT=jit`) compiles
every entry on first use and compares against QEMU like the interpreter tests.

### Networking and 9p

The guest uses v86's own virtio-net, fetch relay and 9p server. The armless
fork fixed these in the shared code, so x86 benefits too:

- **Sliding window.** The relay's TCP sends as many segments as the guest's
  window allows. Before, it sent one segment and waited for its ACK.
- **Recovery.** Duplicate ACKs and a retransmission timer recover lost
  segments, and sequence numbers wrap correctly. A segment larger than the
  guest's window (any MTU above ~64 KB minus headers) used to wedge the
  connection forever.
- **No drops.** Virtio-net queues received frames while the guest has no
  receive buffer posted, instead of dropping them.
- **Uploads.** POST/PUT bodies are joined once instead of being re-copied on
  every TCP segment.

`tests/arm/netfs.mjs` checks integrity on both sides (checksums of downloads,
uploads, parallel requests, 9p reads of zstd blobs, writes the host reads
back, file semantics, files surviving `save_state`) and prints throughput.
In Node:

| | MTU 1500 (default) | `mtu: 65520` |
|---|---|---|
| HTTP download | ~3.6 MiB/s (was 1.6) | ~5.3 MiB/s (used to hang) |
| 9p read / write | ~3.5 / ~2.3 MiB/s | |

Both are bound by guest CPU (the kernel's copies and checksums), not by the
devices. A larger 9p `msize` makes no difference with `cache=loose`.

### Several CPUs

`cpu_count: N` (up to 8) gives the guest N CPUs. The core supports this on
any host: `armless-run --smp N` runs one native thread per CPU. In the browser
and Node, each CPU runs in a worker with `armless-smp.wasm`, the threaded
build: its memory is a shared `WebAssembly.Memory`, and each worker has its
own instance on it, with its own stack, TLS and JIT function table. The
devices stay on the main thread with v86's JS models.

What the CPUs share, and how (`arm/src/smp.rs`):

- **Memory and interrupts.** Guest RAM is shared. The GIC distributor
  (behind a spinlock) has a CPU interface per CPU, banked SGIs/PPIs, SPI
  targets and SGIs (IPIs).
- **Timer and power.** One generic timer epoch for every CPU. PSCI
  `CPU_ON`/`CPU_OFF`/`AFFINITY_INFO`; MPIDR carries the CPU number.
- **Attention byte.** Each CPU has one, polled by its run loop and its JIT
  code. Another CPU sets it to ask for a look at interrupts or shared state.
  A *kick* also wakes an idle CPU (`Atomics.wait` on its wake counter in a
  worker, a condvar natively).
- **TLB shootdowns.** `TLBI ... IS` appends the page (or "all") to a ring
  that the other running CPUs apply at their next instruction boundary. A
  `DSB` waits until they have; idle CPUs apply it when they wake.
- **Atomics and ordering.** `STXR` is an atomic compare-and-swap against the
  value `LDXR` saw (as QEMU does); pairs are updated under a lock. `DMB` and
  `LDAR`/`STLR` become fences.
- **WFE, SEV and YIELD.** `WFE` waits until the watched location changes,
  an interrupt or `SEV` arrives, or a short poll ends. `YIELD` does the
  same. With no more host cores than vCPUs, a waiting CPU gives its host
  thread away instead of spinning.
- **JIT code coherence.** Each CPU has its own JIT. `CTR_EL0` reports
  `DIC=0` and 4 KiB instruction-cache lines, so software issues one
  `IC IVAU` per page it wrote code to; that drops the page's translations on
  every CPU.
- **Devices.** Workers reach them through a mailbox. Reads wait for the main
  thread; writes are posted (arm64 maps device memory Device-nGnRE), except
  PCI config space. `save_state`, reset and `stop()` park every vCPU first.

`tests/linux/smp.c` checks atomics, futexes, a lock-free stack and mmap TLB
shootdowns from 8 threads against `qemu-aarch64`. CI runs it natively with 2
and 4 CPUs, and through the V86 API with 2. The snowglobe flow (9p, console,
`save_state`/zstd restore, fetch networking) also runs with 2 CPUs.

**When it helps.** Parallel, CPU-bound guest work scales (2 CPUs: up to ~2×).
Device I/O from several CPUs pays a worker-to-main-thread round trip per
synchronous device read, so a single-threaded, I/O-bound guest is faster
with one CPU. Use at most `navigator.hardwareConcurrency - 1` CPUs.

## Status

Done:

- [x] A64 integer instruction set, loads/stores, exclusives, system instructions
- [x] EL0/EL1, vectors, SVC/BRK/undefined/aborts, ERET
- [x] MMU: 4K granule, TTBR0/1, blocks/pages, permissions, AF, TBI, AT, DC ZVA
- [x] GICv2, generic timer (virtual + physical), PL031 RTC, PSCI 1.0 over HVC
- [x] FP and Advanced SIMD (ARMv8.0, no crypto), bit-exact with QEMU incl. FPSR
- [x] DTB generator, arm64 Image loader, CPU state save/restore
- [x] Linux 6.8 boots; static glibc and busybox userspace match qemu-aarch64
- [x] armless.wasm (core + v86's zstd decoder), `make armless`
- [x] v86 JS integration: `new V86({ arch: "arm64", ... })` (`src/arm/cpu.js`),
      UART/PCI/virtio devices reused unchanged, gzip and EFI-zboot kernels
- [x] snowglobe flow end to end, also with the Closure-compiled `libv86.mjs`:
      9p root (zstd blobs), virtio console, `save_state`, zstd `initial_state`
- [x] Page JIT to wasm, enabled by default (see "JIT" above)
- [x] Several CPUs: `cpu_count` (workers) and `armless-run --smp N`

Testing (all in CI, `.github/workflows/armless.yml`):

- `cargo test`: unit tests and a bare-metal self-test guest
- `tests/difftest`: random instructions vs `qemu-aarch64 -cpu cortex-a53`,
  natively and on armless.wasm with every entry JIT-compiled (`DUT=jit`)
- `tests/linux`: kernel boot, glibc/busybox userspace vs qemu-aarch64
- `tests/arm/boot.mjs`, `tests/arm/snowglobe.mjs`: through the V86 JS API,
  with the JIT on (`ARMLESS_JIT=0` turns it off, `ARMLESS_STATS=1` prints
  JIT statistics)

Next:

1. **snowglobe**: `FROM arm64v8/alpine`, boot layer installs `linux-virt` for
   aarch64, `new V86({ arch: "arm64", wasm_path: "v86/armless.wasm", ... })`;
   the BIOS files are no longer needed. See "Using it from snowglobe" below.
2. **Performance**: translate more in the JIT (FP/SIMD, exclusives), keep
   guest registers in wasm locals within a block, and cache code translations
   for the dispatcher.

## Using it from snowglobe

```js
const emulator = new V86({
  arch: "arm64",                       // new: selects armless.wasm
  wasm_path: "v86/armless.wasm",
  memory_size: 512 * 1024 * 1024,
  filesystem: { basefs: "system/filesystem.json", baseurl: "system/filesystem/" },
  bzimage_initrd_from_filesystem: true, // finds /boot/vmlinuz-virt (gzip or zboot)
  cmdline: "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose modules=virtio_pci console=hvc0",
  virtio_console: true,
  net_device: { type: "virtio", relay_url: "fetch" },
  initial_state: { url: "system/state.bin.zst" },
  autostart: true,
});
```

Everything else (events, `serial0_send`, `virtio-console0-*`, `save_state`,
`fs9p.storage`, `v86.cpu.devices.virtio_console.virtio.queues`) is unchanged.
The serial console is `ttyS0` (16550 at `0xF0100000`, `earlycon=uart8250,mmio,0xf0100000`).

## Building and testing

```sh
cd arm
cargo test                        # unit tests + bare-metal guest (needs clang/lld)
cargo run --release --bin armless-run -- --raw build-guest/tests.bin
cargo run --release --bin armless-run -- --kernel Image --initrd initrd.cpio
make armless                      # → build/armless.wasm (from the repo root)
node tests/arm/snowglobe.mjs DIR  # DIR from tests/arm/make-9pfs.sh
```
