#!/bin/sh
# What system calls cost a program under a Linux guest kernel (machine mode), the counterpart of
# `cargo bench -p armless-user --bench syscalls` (user mode, no kernel) and bench/syscalls.mjs.
# Builds bench/syscalls-init.c into a static /init (clang and ld.lld, no aarch64 libc needed), packs
# it into an initramfs with /dev/console and /dev/zero, and boots it natively with the native JIT
# (armless-run) and, with --wasm, through armless.js on build/armless.wasm under Node.
#
#   sh bench/machine-syscalls.sh KERNEL [--wasm]
#
# KERNEL: an arm64 Linux Image (gzip, zstd or EFI zboot too), such as Ubuntu's
# linux-image-unsigned-*-generic_*_arm64.deb's /boot/vmlinuz-* from ports.ubuntu.com.
set -eu
cd "$(dirname "$0")/.."
KERNEL=${1:?usage: bench/machine-syscalls.sh KERNEL [--wasm]}
OUT=${OUT:-build/machine-syscalls}
mkdir -p "$OUT"
clang --target=aarch64-linux-gnu -O2 -ffreestanding -fno-stack-protector -fno-pic -nostdlib \
    -fno-builtin -c bench/syscalls-init.c -o "$OUT/init.o"
ld.lld -static -e _start "$OUT/init.o" -o "$OUT/init"
python3 - "$OUT" <<'PY'
import sys
out = sys.argv[1]
ino = [1]
def ent(name, mode, data=b"", rdev=(0, 0)):
    ino[0] += 1
    nb = name.encode() + b"\0"
    fields = [ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(nb), 0]
    e = ("070701" + "".join("%08x" % v for v in fields)).encode() + nb
    e += b"\0" * (-len(e) % 4)
    return e + data + b"\0" * (-len(data) % 4)
init = open(out + "/init", "rb").read()
cpio = (ent("dev", 0o040755) + ent("dev/console", 0o020600, rdev=(5, 1))
        + ent("dev/zero", 0o020666, rdev=(1, 5)) + ent("init", 0o100755, init) + ent("TRAILER!!!", 0))
open(out + "/initrd.cpio", "wb").write(cpio)
PY
CMDLINE="console=ttyS0 rdinit=/init quiet"
cargo build -q --release --bin armless-run
echo "native (armless-run, native JIT):"
ARMLESS_JIT=native ./build/release/armless-run --kernel "$KERNEL" --initrd "$OUT/initrd.cpio" \
    --append "$CMDLINE" -m 512 --timeout 900 | grep '^bench '
if [ "${2:-}" = "--wasm" ]; then
    echo "wasm (armless.js under Node):"
    node tests/js/boot.mjs "$KERNEL" "$OUT/initrd.cpio" --expect "bench done" --cmdline "$CMDLINE" \
        | grep '^bench '
fi
