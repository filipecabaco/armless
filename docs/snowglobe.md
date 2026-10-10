# Using armless from snowglobe

snowglobe builds a Docker image into a static site that boots it in v86. With
armless the same pipeline runs `arm64` images. The JavaScript API, the 9p
filesystem format, the snapshot format and the network relay are unchanged;
only the CPU (and which wasm file is loaded) differs.

This page lists the changes snowglobe needs, file by file, against
`filipecabaco/snowglobe` at the time of writing.

## 1. Runtime files (`internal/tools/Dockerfile`)

Take `libv86.js`, `libv86.mjs` and `armless.wasm` from an armless release
instead of the `v86` npm package. The release tarball keeps the npm layout
(`build/…`), so the copy step barely changes:

```dockerfile
ADD https://github.com/filipecabaco/armless/releases/download/<tag>/v86-armless.tar.gz /tmp/
RUN tar -xzf /tmp/v86-armless.tar.gz -C /tmp && \
    mkdir -p node_modules/v86 && cp -r /tmp/v86-armless/build node_modules/v86/build && \
    mkdir -p site/v86 && \
    cp node_modules/v86/build/libv86.js node_modules/v86/build/armless.wasm \
       node_modules/v86/build/v86.wasm node_modules/v86/build/v86-fallback.wasm site/v86/
```

The x86 machine is still in the same build, so i386 sites keep working. The
BIOS files are only needed for x86 sites.

## 2. Accept arm64 sources (`internal/build/build.go`)

```go
switch img.Architecture {
case "386":
    s.Arch = "x86"
case "arm64":
    s.Arch = "arm64"
default:
    return fmt.Errorf("%s is %s; build it FROM an i386 or arm64 Alpine image", o.Source, img.Architecture)
}
```

`s.Arch` then flows into `run.json`, the page template and the snapshot step
like `Network` and `MemoryMB` do today.

On an x86 host, building and running arm64 images needs QEMU binfmt handlers
(`docker run --privileged --rm tonistiigi/binfmt --install arm64`, or Docker
Desktop, which has them). On Apple Silicon the arm64 build is native.

## 3. Boot layer (`internal/boot/boot.Dockerfile`)

No changes are needed. `BASE` becomes the arm64 image, and `linux-virt`, OpenRC,
agetty on `ttyS0`/`hvc0` and `mkinitfs -F "base virtio 9p"` all work as is.
Pass `--platform linux/arm64` to `docker build` when the base is arm64.

On arm64:

- the kernel is `/boot/vmlinuz-virt`, an EFI zboot image. armless unpacks it.
- `ttyS0` is the 16550 UART that v86's `uart.js` serves, so `serial0_send`
  and `serial0-output-byte` behave exactly as on x86.
- `hvc0`, 9p and virtio-net are the same v86 virtio devices.

## 4. Snapshot and run (`internal/tools/build-state.mjs`, `run.mjs`)

```js
const arm = options.arch === "arm64";
const emulator = new V86({
  ...(arm
    ? { arch: "arm64", wasm_path: path.join(v86Dir, "armless.wasm") }
    : { wasm_path: path.join(v86Dir, "v86.wasm"),
        bios: { url: path.join(biosDir, "seabios.bin") },
        vga_bios: { url: path.join(biosDir, "vgabios.bin") } }),
  memory_size: MEMORY_SIZE,
  bzimage_initrd_from_filesystem: true,
  virtio_console: true,
  ...(options.network === "fetch" ? { net_device: { type: "virtio", relay_url: "fetch" } } : {}),
  cmdline: "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose modules=virtio_pci " +
           "console=ttyS0 init_on_free=on",
  filesystem: { basefs: { url: ... }, baseurl: ... },
});
```

Everything else stays the same:

- the `emulator-loaded` hook on `fs9p.storage`
- `virtio-console0-resize` and the console input pacing through `cpu.devices.virtio_console.virtio.queues[0]`
- `save_state()`, zstd, `serial0_send`
- `preserve_mac_from_state_image` and `network_adapter`

`tsc=reliable` is x86-only. Linux on arm64 ignores it, so the cmdline can stay
shared.

## 5. Page (`internal/site/index.html.tmpl`, `internal/pwa/pwa.go`, `internal/remote/remote.go`)

- `new V86({ arch: "arm64", wasm_path: "v86/armless.wasm", … })` for arm64
  sites. Omit `bios` and `vga_bios`; they are ignored on arm64 anyway.
- The PWA shell list and the remote file list should contain `v86/armless.wasm`
  instead of `v86/v86.wasm` and the two BIOS files for arm64 sites.

## What to expect

- **Compatibility.** CI boots a real Alpine arm64 image built exactly like the
  above (`tests/arm/alpine`), runs commands on the serial shell, and
  snapshots/restores it. `tests/arm/snowglobe.mjs` covers 9p with zstd blobs,
  the virtio console, a zstd `initial_state` restore and fetch networking.
- **Speed.** armless.wasm has a JIT for hot code (on by default, like v86's).
  Cold code such as boot runs in the interpreter, at about 40 MIPS in wasm.
  Hot loops run several times faster. A boot-to-snapshot that takes seconds
  on x86 v86 takes longer, but sites start from the snapshot, so visitors
  only pay for what the app does after that. `disable_jit` and
  `jit_threshold` work as V86 options.
- **Network speed.** `net_device: { type: "virtio", relay_url: "fetch", mtu: 65520 }`
  makes downloads about 1.5× faster than the default MTU of 1500 (the guest
  picks the MTU up from the virtio device). This works on x86 too.
- **Several CPUs.** `cpu_count: 2` (or more) runs each vCPU in a Web
  Worker, with `armless-smp.wasm` next to `armless.wasm`. This needs
  `SharedArrayBuffer`, so the page must be cross-origin isolated. GitHub
  Pages can't send the COOP/COEP headers itself; a service worker such as
  [coi-serviceworker](https://github.com/gzuidhof/coi-serviceworker) can add
  them, using `COEP: credentialless` so the fetch relay can still reach other
  origins. It pays off for parallel, CPU-bound apps (builds, several
  processes); a single-threaded, I/O-heavy app is faster with one CPU. A
  snapshot restores only with the `cpu_count` it was saved with.
  `build-state.mjs` runs in Node, which supports this without any headers.
- **Snapshots.** The snapshot is only valid for the same `memory_size`, as on x86.
  armless refuses to restore an x86 snapshot and vice versa.
