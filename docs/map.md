# Where things are

Where to look to change something, and which tests say it still works. Each module starts with a
`//!` comment saying what it holds; this is the index of them.

## The machine (`src/`)

| To change | Look in |
|---|---|
| An integer, branch, load/store or system instruction | `a64.rs` (system registers: `sysreg.rs`; EL0 access to them: `a64.rs`, `el0_sysreg`) |
| A SIMD (Advanced SIMD) instruction | `advsimd.rs`, `simd.rs`, SIMD loads and stores `simd_ls.rs` |
| A floating-point instruction | `fp.rs` (checked against the host's FPU: `fp/host_fpu.rs`) |
| An AES/SHA/CRC instruction | `crypto.rs` |
| How the interpreter runs a page | `predecode.rs` (decode to ops), `cpu/interp.rs` (the loop) |
| The JIT | `jit/gen.rs` (to wasm; FP in `jit/gen/fp.rs`), `jit/native.rs` (Cranelift, natively), `jit/wasm.rs` (the emitter), `jit/profile.rs` |
| Address translation, the TLB | `mmu.rs`, the fast TLB in `cpu.rs` |
| Exceptions, PSTATE, the CPU's run loop | `cpu.rs` |
| Interrupts, several CPUs | `gic.rs`, `smp.rs` |
| A device | `dev/` (`uart.rs`, `pci.rs`, `virtio.rs`, `console.rs`, `p9.rs` with `fs.rs`, `net.rs` with `netstack.rs`, the web relay `dev/web/`) |
| The memory map, the device tree, guest RAM | `board.rs`, `dtb.rs`, `ram.rs` |
| Snapshots | `vm.rs` (the container, `StateSaver`/`StateLoader`), `state.rs` (CPUs, GIC, timers) |
| Running natively: threads, blobs, egress | `runner.rs`, `native/`, `platform.rs`, `spill.rs` |
| Hardware virtualisation | `cpu/hw.rs`, `cpu/hv.rs` (macOS), `cpu/kvm.rs` (Linux) |
| The browser build | `wasm.rs`, `heap.rs`, `js/armless.js` |

## User mode (`user/src/`): a Linux program with no kernel

| System calls / feature | Look in |
|---|---|
| The dispatcher, one arm a call | `sys/mod.rs` (numbers named in `sys/nr.rs`) |
| read, write, readv/writev, pread/pwrite, sendfile, lseek | `sys/io.rs` |
| open, stat, statx, readlink, getdents, /proc and /dev files | `sys/path.rs`; `/proc/<pid>` in `procfs.rs` |
| mkdir, unlink, rename, link, symlink, truncate, chmod, chown, utimens | `sys/file.rs` |
| close, dup, fcntl, pipe, eventfd, close_range | `sys/fd.rs` |
| ppoll, pselect6, epoll | `sys/poll.rs` |
| ioctl | `sys/ioctl.rs` |
| futex, sched_getaffinity, clocks, getrandom, limits, uname, sysinfo | `sys/misc.rs` |
| What a stalled program waits on | `sys/diag.rs` |
| fcntl and flock locks | `locks.rs` |
| Sockets (AF_UNIX, AF_INET over the machine's network), netlink | `net.rs`, `netlink.rs` |
| mmap, munmap, mprotect, mremap, brk, page faults | `mm.rs` |
| clone, fork, wait, exit, kill of a process | `proc.rs` |
| execve, ELF loading | `exec.rs`, `elf.rs` |
| Processes the host starts (jobs) | `spawn.rs` |
| Signals, sigaction, sigreturn | `signal.rs` |
| Timers, timerfd | `timer.rs`, `timerfd.rs` |
| The terminal | `tty.rs` |
| setuid and friends, groups | `creds.rs` |
| System V shared memory, AIO, the scheduler calls | `shm.rs`, `aio.rs`, `sched.rs` |
| errno values | `errno.rs` |
| Running on several CPUs | `smp.rs` |
| The process state, saved and restored | `lib.rs` (`save`, `restore`; format `MAGIC`), `ser.rs` |
| The host's clocks | `host.rs` |
| armless-exec, the CLI | `main.rs` |
| The browser's user mode | `wasm.rs` |

## Tests

| Test | What it checks |
|---|---|
| unit tests beside each module (`tests.rs` files) | the CPU's instruction groups, the interpreter against `execute`, the native JIT fuzzed against the interpreter, FP against the host FPU, the filesystem, the GIC, the network stack |
| `tests/guest.rs` | a bare-metal guest built with clang, run on the machine |
| `tests/vm.rs` | booting a kernel on a 9p root (needs `ARMLESS_9PFS`) |
| `tests/web.rs` | the web relay against a real TLS client |
| `tests/difftest/` (CI) | single instructions against qemu, natively and through the wasm JIT: int, fp, simd, ldst, crypto, lse |
| `tests/linux/`, `tests/js/` (CI) | Linux userspace in the machine, the browser build |
| `user/tests/exec.rs` | programs as aarch64 machine code: exec, files, memory, signals, the terminal |
| `user/tests/kernel.rs` | what services rely on: ids, processes, /proc, locks, shared memory, saved and restored processes |
| `user/tests/sockets.rs` | sockets |
| `user/tests/errors.rs` | wrong arguments get Linux's errors, never a host panic |
| `user/tests/smoke.rs` | every system call user mode answers, with zero and junk arguments, never panics |

`ARMLESS_TEST_CPUS=4` runs the user-mode tests on four CPUs (CI does, after the one-CPU run).
