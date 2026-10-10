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
 │ 9p.js  virtio.js  virtio_console.js   │      │  A64 interpreter (→ JIT)     │
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

## Status

Done (`arm/`, tested by `cargo test`):

- [x] A64 integer instruction set, loads/stores, exclusives, system instructions
- [x] EL0/EL1, vectors, SVC/BRK/undefined/aborts, ERET
- [x] MMU: 4K granule, TTBR0/1, blocks/pages, permissions, AF, TBI, AT, DC ZVA
- [x] GICv2, generic timer (virtual + physical), PL031 RTC, PSCI 1.0 over HVC
- [x] DTB generator, arm64 Image loader, CPU state save/restore
- [x] Native runner `armless-run`; bare-metal self-test guest (65 checks)
- [x] wasm export layer (`arm/src/wasm.rs`)

Next:

1. **Boot Linux** to an EL0 init with the native runner (kernel config in
   `arm/tests/linux/armless.config`).
2. **FP/AdvSIMD** (scalar FP, NEON integer + FP ops, FPCR/FPSR). Required by
   all real userspace (musl and glibc use NEON for memcpy/strlen).
3. **JS integration**: `src/cpu.js` gains an ARM mode that loads `armless.wasm`,
   creates `mem8` over its RAM, implements the MMIO imports (ECAM, PIO, UART),
   routes `pci.raise_irq` to `armless_set_irq`, and skips x86-only devices
   (PIC, PIT, RTC/CMOS, PS/2, VGA, IDE, floppy, DMA, ACPI).
   Selected with `new V86({ arch: "arm64", … })`.
4. **snowglobe**: `FROM arm64v8/alpine`, boot layer installs `linux-virt` for
   aarch64, cmdline unchanged except the console (`console=hvc0`).
5. **Performance**: a basic-block JIT to wasm, modelled on v86's `jit.rs`
   (block cache keyed by physical page, invalidated on writes to code pages).

## Building and testing

```sh
cd arm
cargo test                        # unit tests + bare-metal guest (needs clang/lld)
cargo run --release --bin armless-run -- --raw build-guest/tests.bin
cargo run --release --bin armless-run -- --kernel Image --initrd initrd.cpio
cargo build --release --target wasm32-unknown-unknown   # → armless.wasm
```
