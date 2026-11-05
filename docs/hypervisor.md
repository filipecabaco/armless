# Running guests in hardware

With the `hypervisor` cargo feature, armless runs a machine's CPUs on the host's own cores where
the host has a hypervisor it can drive, and keeps everything else as it is: the devices, the
board, the runner. Elsewhere (x86_64 hosts, wasm, a container without `/dev/kvm`) the feature
compiles to nothing or falls back, and the interpreter and the JIT run the machine as before.

| host | backend | file |
|---|---|---|
| macOS on Apple silicon | Hypervisor.framework | `src/cpu/hv.rs` |
| Linux arm64 with `/dev/kvm` | KVM | `src/cpu/kvm.rs` |

`src/cpu/hw.rs` is what the two share: the architectural state moving between `Cpu` and a vCPU
(`hw_load` when `run` starts, `hw_store` when it returns), device accesses served by `phys_read` /
`phys_write`, HVC and SMC as PSCI calls (`psci_call`), and the instructions the hardware gives no
syndrome for (a pair, writeback, SIMD&FP to a device), which the interpreter runs one at a time.

## Using it

```sh
cargo build --release --features hypervisor --bin armless-run
ARMLESS_HV=1 build/release/armless-run --kernel Image --initrd initrd --smp 4
```

`ARMLESS_HV=1` asks for it (or `Cpu::enable_hypervisor`); when the host can't, armless says why
and runs as `ARMLESS_JIT` says.

On macOS the binary must be signed with the `com.apple.security.hypervisor` entitlement (an
ad-hoc signature is enough), or Hypervisor.framework refuses to make the VM:

```sh
cat > hv.entitlements <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>
EOF
codesign -s - -f --entitlements hv.entitlements build/release/armless-run
```

On Linux the process needs read and write access to `/dev/kvm`. Containers usually have none
(OrbStack's have none, even privileged); armless then falls back.

## The interrupt controller

armless emulates a GICv2. With Hypervisor.framework armless keeps it: the guest's GIC accesses
come out as device accesses, and armless raises the vCPU's IRQ line.

KVM has no GICv2 to offer on GICv3 hosts (all recent ones), so armless drives each vCPU's IRQ
line itself (`KVM_IRQ_LINE`). Under some nested hypervisors (Apple's, which Lima VMs on a Mac use,
and likely cloud VMs) that line wakes the vCPU but the guest never takes the interrupt. So
before a boot, `hypervisor_probe` runs a guest of a few instructions that waits for that interrupt:

- taken: armless's GICv2, as with Hypervisor.framework;
- not taken: KVM's in-kernel GICv3 (`Machine::gic_v3`). The device tree describes a GICv3 (its
  redistributors from `board::GICR_BASE`), the devices' lines go to KVM as SPIs
  (`Machine::irq_sink`), and KVM handles the timer and the interrupts between CPUs itself.

A machine on KVM's GICv3 can't be saved yet: its interrupt controller's state is in the host's
kernel, which a snapshot doesn't hold (`StateSaver::new` refuses).

## What it gave

The Alpine kernel and initramfs of a snowglobe site, booted with `armless-run`; a shell loop of
300 000 iterations and `md5sum` of 64 MiB, timed by the guest's clock:

| | the JIT | in hardware | |
|---|---|---|---|
| macOS (M5 Pro), loop | 7.42 s | 0.22 s | 34× |
| macOS, `md5sum` | 0.66 s | 0.11 s | 6× |
| Linux in a Lima VM (nested KVM), loop | 8.19 s | 0.22 s | 37× |
| Linux in a Lima VM, `md5sum` | 0.68 s | 0.13 s | 5× |

Boot reaches `/init` at 0.08 s of guest time with Hypervisor.framework, 0.61 s with the JIT.
Under nested KVM what exits to armless (device accesses: the UART during boot) is slow, as every
exit goes through two hypervisors; on a host without one in between it is far cheaper.

## The counter

The guest's generic timer counts at a frequency it reads once, at boot, and keeps. armless emulates
any frequency (`Machine::timer_freq`, 62.5 MHz by default), but hardware has one: the host's
(24 MHz on Apple silicon; others differ). So:

- a machine that boots in hardware takes the host's frequency;
- a snapshot carries its frequency (snapshot format version 3, written only for one other than the
  default, so older readers still take default ones), and a restore takes it;
- a restored machine goes on in hardware only at the host's frequency (and with armless's GIC):
  otherwise armless says why and runs it with the interpreter and the JIT, whose counter keeps
  the snapshot's rate. To make snapshots that resume in hardware, make them in hardware (or on a
  host with that counter).

In hardware the guest's counter goes on from armless's: the offset Hypervisor.framework's virtual
timer (against `mach_absolute_time`) or KVM's VM-wide counter offset is given comes from the
machine's timer bases (`Cpu::counter_offset`), the same for every CPU.

A restored guest's wall clock starts at its snapshot's moment; snowglobe's jobs set it to the
host's time as they start, as after a resume from suspend.

## The CPU's features

A guest in hardware is shown armless's own feature ID registers (`hw::MASKED_ID_REGS`): without
pointer authentication, BTI, MTE, SVE, SME or FP16 it boots and behaves as under the emulator, and a
snapshot made in hardware goes on in either. (Unmasked, a guest booted on an M5 signs its return
addresses with keys a snapshot doesn't hold, and dies wherever it is restored.) The cache's
registers (CTR_EL0, CLIDR_EL1, DCZID_EL0) stay the host's: they say what maintenance the hardware
needs.

Hypervisor.framework sets all of them but ID_AA64ISAR2_EL1 (macOS 27). KVM refuses fields it
doesn't let change: armless lowers them one at a time, and says which stay the host's. Linux 6.8
keeps FP16 and BTI: a guest started there may use half-precision instructions armless can't run.

## Not yet

- A machine on KVM's GICv3 can't be saved (its GIC is in the kernel).
- Hypervisor.framework's own GICv3 (macOS 15 and later) is unused.
- A bare-metal guest that uses exclusives with the MMU off (the self-test guest's `atomics`)
  spins: real hardware does not promise exclusives on Device memory.
