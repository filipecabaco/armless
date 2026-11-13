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
| `cpu.rs`, `a64.rs`, `advsimd.rs`, `crypto.rs`, `fp.rs`, `simd*.rs`, `mmu.rs`, `sysreg.rs` | the CPU: state, decode and execute, the MMU and system registers |
| `predecode.rs`, `cpu/interp.rs` | the interpreter: each code page decoded once to ops, and the loop that runs them |
| `jit/` | hot code pages translated to wasm (`gen.rs`): run as wasm in the browser, compiled to host code with Cranelift natively (`native.rs`, the default); `profile.rs` carries what a machine compiled to the next |
| `cpu/hw.rs`, `cpu/hv.rs`, `cpu/kvm.rs` | the CPU in hardware (`hypervisor` feature): Hypervisor.framework, KVM |
| `gic.rs`, `rtc.rs`, `smp.rs` | shared machine state, multi-CPU GIC, PSCI, TLB shootdowns |
| `board.rs`, `dtb.rs`, `ram.rs` | the board's memory map and its device tree, guest RAM |
| `dev/` | UART, PCI, virtio transport, console, 9p + filesystem, net + network stack, the web relay (`dev/web/`) |
| `vm.rs`, `state.rs` | `Board` (devices + host API), `Vm` (with CPUs), snapshots and their CPU/machine state |
| `runner.rs`, `native/`, `platform.rs`, `spill.rs` | native hosting: threads, blob directory, egress, TCP NAT, decompressed blobs on disk |
| `kernel.rs` | a distribution's arm64 kernel made a raw `Image` |
| `wasm.rs`, `heap.rs`, `js/armless.js` | the browser build |
| `../user/` | user mode: a Linux program with no guest kernel (`armless-user`, `armless-exec`); its `wasm.rs` adds user mode's exports to armless.wasm |

[map.md](map.md) says where each feature and each system call lives, and which tests cover what.

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
held) and is answered when the blobs it needs have all arrived. In the browser,
fetched blobs are cached up to 256 MiB, oldest evicted first, except the chunks a
waiting read covers. A native host decompresses each blob once into an unlinked
temporary file (mode 0600, in the temporary directory) and reads it with pread, so
the blobs take no heap and are never decompressed twice. When the temporary directory
is tmpfs or ramfs, or its file system cannot be told, or the file cannot be made or
fills up, the blobs stay in the cache above (the host prints one warning for a file
that fills up).

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

### Waiting for another CPU

A CPU waits for another in three places: a DSB after a broadcast TLBI (until every running CPU
applied it), the guest's WFE (its spinlocks, and software polling), and a `SpinLock` of
armless's own (the GIC, the RTC, the TLB and code rings). How it waits is `Machine::policy`:

- **Adaptive** (the default): a TLB wait spins for `SPIN_NS` (50 µs), then gives up its host
  thread now and then; a guest's WFE wait (followed over the many WFEs of its loop, and ended
  when the CPU goes idle) does the same once it has lasted that long; a `SpinLock` gives up its
  thread after 128 spins, on a thread that may block (a vCPU worker, not a page's own thread).
  A TLB wait of `SLOW_WAIT_NS` (2 ms) or more means the CPU waited for had no host core, and
  after three the machine counts as oversubscribed whatever the host reported, so waits give
  up their thread almost at once. (Only TLB waits teach this: a guest may wait in WFE for
  seconds on purpose.)
- **Spin-count** (armless 0.12): spin a set number of times (100,000 for a TLB wait, 4,000 for
  a WFE), fewer when the host reports no more cores than CPUs.

What the waits cost is in the memory report's `smp` (below), with how many instructions each
CPU ran.

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

A worker compiles a JIT block in place: `new WebAssembly.Module` and `new WebAssembly.Instance`,
which a worker may do at any size (a page's own thread may only for small modules). The block
is marked ready as soon as `armless_run` returns, with no turn of the event loop. A compilation
the browser refuses that way goes through `WebAssembly.instantiate`, which finishes only when
the worker's event loop runs; a worker waiting in `Atomics.wait` or running the guest doesn't
run it. So a worker with such compilations under way lets its event loop run (a posted message
to itself) every 4 ms, and one with none doesn't.

Why it matters: a turn of a worker's event loop costs well under a millisecond in Chrome but
around 25 ms in Safari on an iPhone. Workers that yielded every 4 ms whatever they did spent
about 80% of their time there, and a 2-CPU Supabase couldn't create a table within 90 s. With
yields only while compilations were under way, the same machine ran each step of its setup in
1 to 2 s, and a 3-million-row query in 34 s (43 s with the fixed spin count); the 1-CPU site,
which has no workers, ran it in 15 s. Compiling in place takes the remaining yields away: on a
desktop, a boot and the same steps compiled about 3,000 blocks with no yield at all.

Each worker counts where its time went in a shared buffer: running the guest, waiting idle,
yielding, slices, idle waits, events posted to the page, yields and compilations. The page
counts how late the workers' event messages reach it (a device request waits that long).
`memory_report().smp` and `smp_report()` give them with the waits above.

## User mode (experimental)

`user/` (the `armless-user` crate, and `armless-exec` on the command line) runs a Linux program
with no guest kernel. The machine is the same; what is missing is EL1. The CPU runs the program
at EL0 with no vector table and `Cpu::stop_on_exceptions` set, so every exception it takes stops
the CPU (`StopReason::UnhandledException`, cpu.rs) with ESR, ELR and FAR, whether the MMU is on
or off. That is true in the interpreter and in both JITs. The host reads the syscall from x8 and
x0–x5, answers it in x0, and resumes at ELR.

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
  few `/proc` files, are made here. File mappings (`mmap` of a file) are private copies, filled
  lazily (below).
- **The terminal:** stdin, stdout and stderr are a terminal with termios (TCGETS, TCSETS), a
  window size (`Process::resize`, TIOCGWINSZ) and a line discipline, as n_tty: in canonical
  mode a line is edited and echoed before it is read; in raw mode bytes pass through; with
  ISIG, Ctrl-C, Ctrl-\ and Ctrl-Z are SIGINT, SIGQUIT and SIGTSTP, flushing the input and the
  output not taken yet (unless NOFLSH) and echoing `^C`; a new size is SIGWINCH; output
  newlines start a line (ONLCR).
- **Pipes, eventfd, epoll** (edge-triggered and one-shot too), **poll and select.**
- **File locks** (user/src/locks.rs), as Linux keeps them: record locks (`fcntl` F_GETLK,
  F_SETLK, F_SETLKW) are a process's, on byte ranges, and go when it closes any descriptor of the
  file or ends; OFD locks are a description's and conflict with them; `flock` is a description's
  whole-file lock. SQLite's locks hold, and the Supabase CLI tells its stack's owner is running by
  one.
- **Sockets** (user/src/net.rs): `socket`, `socketpair`, `bind`, `listen`, `accept` and `accept4`,
  `connect`, `sendto` and `recvfrom`, `sendmsg` and `recvmsg` (and the `mmsg` pair), `shutdown`,
  `getsockname`, `getpeername`, `getsockopt` and `setsockopt`; a socket is a descriptor like a
  pipe's, and read, write, readv, writev, fcntl, poll, select and epoll take it. Blocking and
  non-blocking (`SOCK_NONBLOCK`, `EAGAIN`, `EINPROGRESS`) as for pipes: a call that can't finish
  parks its thread and runs again when something changed. `FIOASYNC` (`O_ASYNC`) and `F_SETOWN`
  are accepted, and SIGIO is never sent.
  - *In memory:* AF_UNIX stream and datagram sockets (paths, which are entries of the
    filesystem, and abstract names, and `socketpair`), and AF_INET and AF_INET6 TCP and UDP to
    the loopback (127/8, `::1`, the machine's own address, an unspecified one), through a table of
    listening ports the process keeps. Streams buffer 64 KiB for the reader, datagram queues hold
    128; a stream's end that closes gives the other EOF, a write to it EPIPE, a connect to a
    port nobody listens on ECONNREFUSED. Ports an unbound socket gets start at 49152.
  - *The host:* on a machine with a network, a connection to any other address is a host
    stream (`Event::TcpOpen`, with the name the DNS answer gave for a 10.x address, else the
    dotted address), accepted, refused or carried over a tunnel as for a Linux guest; `connect`
    waits for the answer. A guest listener is reachable from the host (`Board::tcp_connect(port)`,
    which `armless-exec --publish [ADDR:]HOSTPORT:PORT` and any host's forward use):
    the machine keeps the listening ports and the connections made to them (core,
    `dev/netstack/local.rs`), not the guest netstack's SYNs. A UDP datagram to port 53 of a
    non-local address is a DNS query the network stack answers (`Board::local_dns`), so
    resolv.conf's nameserver works; other UDP to the outside and all IPv6 to the outside fail
    with ENETUNREACH.
  - *Processes:* the sockets are the machine's, like the pipes: a descriptor refers to one
    through its description, so a fork's child has its parent's sockets (a connected one, a
    listener it can `accept` on too), `execve` keeps them unless they are close-on-exec, and a
    call that waits on one (of any process) runs again when it changes.
  - *Snapshots:* the sockets' state is in the machine's, streams and queues with their bytes,
    whatever process holds them. A connection to the host can't survive a restore:
    it reads as closed, and one that was connecting as reset.
  - *Ancillary data* goes between unix sockets, streams and datagrams. `SCM_RIGHTS` passes
    descriptors (Erlang hands its port programs' pipes to `erl_child_setup` so): a datagram
    carries them, a stream the bytes of the send that had them, each holding a reference until
    received; `recvmsg` installs them (MSG_CMSG_CLOEXEC, MSG_CTRUNC when its room is short), a
    peek gives copies, and a read or `recvfrom`, which takes no control data, closes them. A stream
    read that reaches a send carrying descriptors takes them and ends with that send, as Linux's.
    With `SO_PASSCRED` a socket receives `SCM_CREDENTIALS`, who sent what it reads (pid, uid, gid,
    or what the sender gave, as root may; a pid that isn't a process's is ESRCH). An unbound unix
    client is accepted as its family alone (Postgres's "local" lines match so).
  - *Interfaces* (user/src/netlink.rs): a netlink route socket answers the RTM_GETLINK,
    RTM_GETADDR and RTM_GETROUTE dumps (`ip addr`, `ip route`, getifaddrs, Node's
    `os.networkInterfaces()`, Go's `net.Interfaces()`, Erlang's `inet:getifaddrs()`): `lo`
    (127.0.0.1/8, ::1/128) and, with a network, `eth0` at 192.168.86.100/24 with the default
    route via 192.168.86.1; a change is EPERM. Any socket answers the SIOCGIF* ioctls from the
    same table (`ifconfig`), and `/proc/net/dev` lists the interfaces.
  - *Not supported:* descriptors to the host (`EOPNOTSUPP`), other ancillary data of a unix
    socket (EINVAL), `SOCK_SEQPACKET`, raw and packet sockets but netlink's route family,
    `socketpair` for anything but AF_UNIX, a separate `RDHUP`, backpressure from the guest to the
    host (a host stream's data is taken as it comes).
- **Memory** (user/src/mm.rs): virtual. Each process has page tables of its own (4 KiB pages,
  39 bits: 512 GiB of address space, an ASID per process so a switch flushes nothing) over one
  pool of refcounted frames, the machine's RAM; the tables are in frames too, which no process
  maps. A process's areas say what each range is: its protection, whether it is shared, and the
  file its pages come from if any. The ELF's segments go where they ask (a PIE, and the loader,
  at a base of their own), `brk` after them, the stack at the top of the space and `mmap` below
  it, from a free list; `mremap` moves or grows by moving page table entries, not pages.
  Protections hold: a store to a read-only page, a jump to one that is not executable and any
  access to PROT_NONE are faults, and a system call cannot write where the program cannot
  (EFAULT). The address space has nothing to do with the RAM: a page has a frame when something
  first touches it (a translation fault stops the CPU, the host takes a frame, zeroed or filled
  from its file, maps it, and resumes at the instruction). So a reservation costs nothing: a
  PROT_NONE or MAP_NORESERVE mapping of 8 GiB on a machine of 128 MiB (what V8's heap cage and
  WebAssembly memories ask for, and Go's 600 MB) takes no frame, `mprotect` and touches take
  frames where the program uses them, and `munmap` and `madvise(MADV_DONTNEED)` give them back
  (`munmap` the page tables with nothing left in them too; a frame's memory goes back to the
  host with `Ram::discard`). There is no overcommit check: a program that touches more than the
  RAM has is stopped when the pool runs dry, not when it maps.
- **Lazy file pages:** the executable's segments, its loader's, and every `mmap` of a file are
  areas with a file behind them, not read: a page gets its contents when something first touches
  it. So a snapshot holds the pages a program used, not every byte of every library it mapped,
  and only their blobs are fetched. The fault the touch is stops the CPU; the host fills the page
  from its file (or the thread waits for its blobs, as a read does, and touches it again), maps
  it and resumes. A system call that reads or writes a lazy page (a `write` from a mapped page)
  fills it first. A MAP_PRIVATE mapping is a copy: a write gets the page a frame of its own and
  the file keeps its contents. A MAP_SHARED one is the file's: every mapping of a page of it, in
  any process, has the one frame (`Process::file_pages`, Postgres's dynamic shared memory), and
  the page is written back to its file once no mapping holds it. The areas' bookkeeping is in
  the process state. POSIX shared memory (`shm_open`) is files in `/dev/shm`, a directory of the
  machine's filesystem a program's start makes if the image has none (Supabase's Postgres keeps
  its dynamic shared memory there), and `fallocate` sizes a file (mode 0, and KEEP_SIZE).
  `ARMLESS_USER_EAGER=1` reads every mapped page at once, for comparisons.
- **Threads:** every guest thread is a saved register context, and they take turns on one
  CPU in slices of instructions; a syscall answered on the way counts against the slice.
  `clone` (threads, CLONE_THREAD), futexes (wait and wake, with timeouts and bitsets; requeue and
  wake-op, which musl's condition variables use), `set_tid_address` and
  `CLONE_CHILD_CLEARTID`. A futex is its process's address, unless it is in a shared area and
  the call has no FUTEX_PRIVATE_FLAG: then it is its frame's, and waits and wakes meet across
  processes (Postgres's semaphores are in its shared memory).
- **Scheduler calls** (user/src/sched.rs): `sched_setscheduler`, `sched_getscheduler`,
  `sched_setparam`, `sched_getparam`, `sched_setattr`, `sched_getattr`, `sched_get_priority_max`
  and `_min`, `sched_rr_get_interval`, `setpriority` and `getpriority` answer as Linux does for a
  process that may do them. Policy, priority and nice are kept per thread (PRIO_PROCESS acts on a
  tid, PRIO_PGRP and PRIO_USER on every matching thread), inherited by `clone` (SCHED_RESET_ON_FORK
  resets the child to SCHED_OTHER and a nonnegative nice) and kept by exec, and they are in the
  process state. SCHED_OTHER, BATCH and IDLE take priority 0, FIFO and RR 1 to 99; they are only
  kept and reported, the CPU is still shared by round robin. SCHED_DEADLINE is refused (EINVAL
  from `sched_setscheduler`, EPERM from `sched_setattr`). `sched_rr_get_interval` says 100 ms for
  SCHED_RR and 0 for the others. `getpriority` returns `20 - nice`, the raw call's convention.
- **Processes** (user/src/proc.rs, exec.rs): a process is an address space, a descriptor table,
  a working directory, signal actions and its threads; the descriptions the descriptors refer
  to, the pipes, the terminal and the clock are the machine's, shared by all, so a pipe joins a
  parent and its child. `clone` makes one with `fork`'s flags, as `vfork` (the parent waits until
  the child execs or ends) or with CLONE_VM, and `clone3` too; the address space is copied on
  write (the tables are copied, not the pages: both sides map the frames read-only, and the
  first write by either takes a frame of its own, or makes the page writable again when the
  other side is gone). `execve` and `execveat` (an fd, AT_EMPTY_PATH) run another program in the
  same process: a new address space, the descriptors that are not close-on-exec, handled
  signals back to the default, the other threads gone, and a `#!` script runs its interpreter (so
  does the first program, `Process::exec`: an image's command is often a script); a
  failure (ENOENT, EACCES, ENOEXEC, E2BIG, ELOOP) leaves the program as it was, and one whose
  file hasn't arrived waits for it and runs again. `wait4` and `waitid` (WNOHANG, a pid, a
  group or any child; a zombie keeps its status), `kill` of a pid, and SIGCHLD when a child
  ends (a parent that ignores it leaves no zombies). A process's `exit_group` ends only it; its
  children go to the first process, and the machine ends when that one does. Not yet: the
  descriptor table, signal actions and working directory shared by CLONE_FILES, CLONE_SIGHAND
  and CLONE_FS (copied instead), futexes in the private memory of CLONE_VM processes (they wake
  only within a process), the siginfo of SIGCHLD (its pid and status), job control (stop and
  continue), namespaces, and an anonymous MAP_SHARED area that was PROT_NONE at a
  fork shares nothing (the others have their untouched pages made at the fork).
- **Users** (user/src/creds.rs): a process has real, effective and saved user and group ids,
  root to start with. `setuid`, `setgid`, `setreuid`, `setregid`, `setresuid` and `setresgid`
  change them as Linux does (root to anything, anyone else among its own), `getuid` and the
  rest say them, a child has its parent's and `execve` keeps them, and what a process makes
  (files, directories, links, sockets' names) is owned by its effective ids. `chown` and
  `fchownat` set a file's owner (root to anyone, its owner to one of its groups). A process has
  supplementary groups too: `setgroups` (root's) keeps a list `getgroups` gives, a child
  inherits and exec keeps. So `su` and Postgres's `pg_ctl` (which refuses to run as root) work.
  Permissions are not checked by them.
- **System V shared memory** (user/src/shm.rs): `shmget`, `shmat`, `shmdt`, `shmctl` (IPC_STAT,
  IPC_SET, IPC_RMID). A segment is the machine's: it has frames of its own, each attach maps
  them as a shared area, and a fork shares them; it goes once removed and detached.
- **signalfd:** `signalfd4` gives a descriptor whose reads take the pending signals of its mask,
  which the program blocks (a `signalfd_siginfo` each, a timer's with its id and overrun),
  readable to poll and epoll.
- **Timers** (user/src/timer.rs, timerfd.rs): the interval timers (`setitimer`, `getitimer`:
  ITIMER_REAL raises SIGALRM; ITIMER_VIRTUAL and ITIMER_PROF, SIGVTALRM and SIGPROF, from the
  CPU time of the process, the host time its threads held the CPU) and the POSIX timers
  (`timer_create`, `timer_settime` with TIMER_ABSTIME, `timer_gettime`, `timer_getoverrun`,
  `timer_delete`: SIGEV_SIGNAL, SIGEV_THREAD_ID, SIGEV_NONE, the SI_TIMER siginfo and overruns)
  on the wall, monotonic, boot and CPU-time clocks (`CLOCK_PROCESS_CPUTIME_ID`,
  `CLOCK_THREAD_CPUTIME_ID`, for `clock_gettime` too). A child has no timers; exec keeps the
  interval timers and deletes the POSIX ones. A timerfd (`timerfd_create`, `timerfd_settime`,
  `timerfd_gettime`; GHC's runtime, Supabase's PostgREST, makes one) counts its expirations:
  a read gives them, poll and epoll see one that fired. An idle machine's host comes back when
  the next timer is due.
- **Process descriptors:** `pidfd_open` and `CLONE_PIDFD` (clone and clone3) give a descriptor
  of a process, readable once it has ended and hung up once it was waited for; `pidfd_send_signal`
  signals it, `pidfd_getfd` takes a description of its table, `waitid(P_PIDFD)` waits for it (one
  with PIDFD_NONBLOCK never waits), and `/proc/self/fdinfo/N` says its pid (the Supabase CLI
  watches its stack's owner so).
- **AIO** (user/src/aio.rs): `io_setup`, `io_destroy`, `io_submit`, `io_cancel`, `io_getevents`
  and `io_pgetevents`, done as they are submitted (reads, writes, their vectored forms and fsync),
  their completions signalled to an eventfd with IOCB_FLAG_RESFD (nginx's `aio on`). `preadv2`
  and `pwritev2` take RWF_NOAPPEND, RWF_APPEND and RWF_DONTCACHE (RWF_ATOMIC is EOPNOTSUPP), and a
  positioned write to a descriptor opened O_APPEND goes to its end, as Linux's.
- **CPU identification:** a read of an ID register (`mrs` of MIDR_EL1, MPIDR_EL1, the ID_AA64*
  registers), which EL0 can't make, is answered with the CPU's value as Linux answers it for a
  program (HWCAP_CPUID); Go's `x/sys/cpu` reads ID_AA64ISAR0_EL1 as it starts.
- **/proc/self:** `exe` is the file that runs with its links followed (musl's loader finds
  `$ORIGIN` from it: `/usr/bin/java` links into the JDK); `fd/N` says what each descriptor is open
  on, as Linux does (a path, `pipe:[N]`, `socket:[N]`, `anon_inode:[...]`), for realpath in Zig,
  Bun and Deno; `fdinfo/N` its position and flags.
- **Blocking:** a call that can't complete (a read of the terminal or a pipe, a poll, a wait
  for blobs) parks its thread with its pc on the svc, and runs again when something changes,
  or returns at its deadline. So whatever a program waits on, a snapshot can be taken.
- **Clocks:** `clock_gettime`, `gettimeofday`, `nanosleep`, `clock_nanosleep`. The monotonic clock goes on
  across a snapshot and its restores.
- **Signals** (signal.rs): actions per process, a mask, a pending set and an alternate stack
  per thread, a pending set for the process (given to its first thread that doesn't block the
  signal, the main thread first). A handler runs on the `rt_sigframe` the kernel builds
  (siginfo, ucontext, sigcontext with an fpsimd_context record), on the alternate stack with
  SA_ONSTACK, and `rt_sigreturn` takes the thread back from it, mask and FP/SIMD registers too.
  With no SA_RESTORER (Go on arm64) the handler returns to a page of the process's with the
  vDSO's two instructions. A thread parked in a call is woken for a signal it handles: reads
  and writes of the terminal and pipes, and futex waits with no timeout, start again with
  SA_RESTART and return EINTR without; poll, select, epoll, sleeps (with the time left),
  `rt_sigsuspend` and `rt_sigtimedwait` return EINTR. A wait for blobs is not interrupted
  (a disk wait): its signal comes once it ends. SIGCHLD, SIGCONT, SIGURG, SIGWINCH and the
  stop signals are ignored by default (no job control), the rest end the process with 128 +
  the signal. `Process::kill` is a signal from the host. Go's SIGURG preemption is delivered.
  Not yet: queued real-time signals, the masks of ppoll, pselect6 and epoll_pwait, and
  signals for faults.
- **Snapshots:** the machine's own snapshot holds RAM and the filesystem; `Process::save` holds
  the rest (the processes, their threads and waits, their memory maps and lazy mappings, descriptors
  and pipes, the terminal, the clock, signals), a few tens of KB; the frames and page tables
  are in the machine's RAM. `Process::restore` takes it on a machine that loaded the snapshot.
  The state is `ARMLUSR7`: 0.15's `ARMLUSR6` with each thread's scheduling, each process's
  groups and AIO contexts, the armed timers (CPU time counted so far, and the POSIX ones), the
  descriptors and senders of what waits at sockets, and timerfds; `ARMLUSR6` is 0.14's
  `ARMLUSR5` with each process's ids and interval timer, the System V segments, the shared file
  pages, and the descriptors on their way over sockets. Both older ones load (what they lack
  starts as a new process has it); a state from before 0.14 does not (addresses are virtual,
  there are processes, and the sockets are in the machine's state).
- **The host:** `host.rs` is all user mode takes from the host besides the machine: a monotonic
  clock (`Instant`), the wall clock and randomness. Natively they are std's and the system's;
  in WebAssembly, whose std has no clock, they are armless's JavaScript imports (`microtick`,
  `wall_clock_ms`) and a SHA-256 generator keyed by the host's entropy (the last
  `armless_seed`, which armless.js gives after every restore).

### Several CPUs

On one CPU the threads of every process take turns (`Process::run`). On several (`Smp`, or
`Process::run_on` with all the machine's CPUs; `armless-exec --cpus N`) they run at the same
time: each CPU runs on a host thread of its own and runs guest code with no lock held. What a
thread asks of its kernel (a system call, a page it touches first, a fault) stops its CPU,
which takes the process's lock and answers it with the same code one CPU uses
(`Process::answer`); picking a CPU's next thread is under the lock too (`Process::schedule`),
and a CPU takes only a thread no other CPU has. Atomics need nothing more: a store-exclusive is
armless's, as with a kernel, and futexes are answered under the lock.

What one CPU does that another must see before it goes on:

- **A change to an address space** (unmapped, protected, copied on write, a fork): the
  translations every CPU holds of what changed go before the call returns, by armless's
  broadcast TLB invalidation (`Cpu::tlb_shootdown`: the other CPUs drop them at their next
  instruction, and the call waits for those in guest code).
- **A process that ends or execs:** its threads on other CPUs stop there first
  (`Machine::preempt` makes a CPU's `run` return at its next instruction), and their CPUs are
  out of guest code (`Cpu::left_guest`) before its memory is freed (`take_off_cpus`).
- **A signal for a thread another CPU runs:** that CPU is stopped, and delivers it itself, its
  registers being on it.

A CPU with nothing to run waits on a condition variable that a CPU making a thread runnable
signals. A program is told of the CPUs there are (sched_getaffinity, `/proc/cpuinfo`; four at
least, as on one CPU) and getcpu says which it is on. A snapshot is taken with every CPU's
thread back in the process (`Smp::pause`), so it is the same whatever number of CPUs made it,
and restores on any.

Programs that do work in parallel go as much faster: four processes of Python 3.6 times, a
DuckDB table scan 3.7 times, on four CPUs. A start that waits on itself does not: Supabase's
stack is ready in 23 s rather than 25 s, its four CPUs running guest code a quarter of the time,
the rest of it a chain of starts and the polls between them.

### In the browser

armless.wasm is the cdylib of `armless-user`, which depends on armless, so one module has both
export sets: armless's `armless_*` (the `#[no_mangle]` functions of a dependency are exported
from a cdylib as they are) and user mode's `armless_user_*` (`user/src/wasm.rs`). These run on
the machine `armless::wasm` keeps, with its `vm()`, `out()`, `set_error()` and pointer helpers.
armless-smp.wasm is still built from armless alone: in a page, user mode runs on one CPU. User mode adds
about 290 KB to armless.wasm (1.28 to 1.57 MB).

A host makes the machine as for Linux (`armless_create`, `armless_fs_load`, `armless_seed`)
and then, instead of a boot:

| export | |
|---|---|
| `armless_user_exec(argv, len, env, len, cwd, len)` | start a program (argv and env NUL-separated): 0 started, 1 its files are being fetched (blob events: provide them, call again), -1 failed |
| `armless_user_restore(ptr, len)` | the process of a snapshot (`Process::save`'s bytes), after `armless_state_*` restored the machine |
| `armless_user_run(budget_ms)` | 0 its time ran out, 1 idle, 2 exited, 3 faulted (the reason in `armless_error`), -1 no process |
| `armless_user_wake_ms()` | after 1: the `microtick()` time of its next deadline, or -1 (only input or a blob wakes it) |
| `armless_user_waiting_on_blobs()` | 1 when a thread waits on a blob rather than on input |
| `armless_user_waiting_for_input()` | 1 when a thread waits in a call input could complete (read, poll, epoll) and none on a blob: a REPL at its prompt, not a program asleep |
| `armless_user_exit_code()` | the exit status, -1 while it runs |
| `armless_user_input(ptr, len)`, `armless_user_close_input()`, `armless_user_resize(cols, rows)` | the terminal |
| `armless_user_kill(sig)` | a signal for the program, as kill(2) sends it (SIGINT for a stop button) |
| `armless_user_save()` | the process's state, at `armless_buf` |

What the program writes comes out of `armless_events` as console output, and the files it
reads that aren't there yet as blob events, so the event loop a host has for Linux serves it
too. A snapshot is the pair a native host makes: the machine's state, the bytes of
`Vm::save_state` (`armless_save_*`), and the process's, those of `Process::save`
(`armless_user_save`), restored machine first. A site snowglobe saved natively
(`state.bin.zst` and `user.bin`) restores in a page, and a pair saved in a page restores
natively.

Not yet: signals for faults (a fault ends the process, SIGSEGV is not delivered to a
handler), more than one vCPU in a page, and the hypervisor backends (an SVC at EL0 traps to EL1 in
hardware there, not to the host).

What runs, from snowglobe's examples' own filesystems: `apk`, `curl` (HTTPS through the host),
`duckdb` (C++), `go` (Bubble Tea, static), `java` (OpenJDK 21), `python` (the 3.14 REPL, 122
shared libraries, subprocesses), `python-agent`, `rust` (a static binary), `typescript` (tsx on
Node), `iex` (Erlang/OTP, with port programs), `postgres` (`su`, `pg_ctl`, the postmaster and its
processes, queries through psql: 100,000 rows in 1.2 s), `elixir` (a release talking to that
Postgres) and `supabase` (Postgres, PostgREST, Auth, Realtime and nginx from its own launcher:
built as a snowglobe site with `--kernel none`, ready 24 s after it starts, signup, row-level
security and the API on the machine's network address checked); a Python REPL saved with its
state and restored in 120 ms. `user/corpus/survey.exs` traces what each of snowglobe's
examples asks of a kernel, for what comes next.

Measured as a snowglobe site (a Go program with eight goroutines, macOS, the same
`snowglobe-vm` for each): the snapshot is 2.5 MB raw and 0.6 MB compressed, against 22.1 and
3.6 MB under a stripped Linux and 50.6 and 10.2 under Alpine's `linux-virt`; a machine's
footprint is 22 MB, against 57 and 136.

Lazy file pages, measured the same way on snowglobe's examples (compressed snapshot, before and
after): `duckdb` 15.6 MB to 4.9 MB (64 to 24 MB raw), `python` 4.3 to 3.1 MB, `go` 1.3 to 0.9
MB. Each answers its first command after a restore as fast as before when its host runs the
process again as soon as blobs arrive (pages first touched then are filled from the
filesystem).
