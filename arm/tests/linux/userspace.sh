#!/bin/sh
# Boot Linux with the busybox/glibc rootfs on armless and compare the output
# of the userspace tests with the same binaries run under qemu-aarch64.
#
#   tests/linux/userspace.sh [OUT_DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/../../build-linux}
cd "$here/../.."
cargo build --release -q --bin armless-run

# Reference
qemu-aarch64 -cpu cortex-a53 "$out/userspace" > "$out/ref.txt"
(cd "$here/rootfs" && qemu-aarch64 -cpu cortex-a53 "$out/busybox" sh tests.sh) | grep -v '^dc' >> "$out/ref.txt"

# armless
../build/release/armless-run -m 512 --kernel "$out/Image" --initrd "$out/rootfs.cpio" \
    --append "console=ttyS0 rdinit=/init quiet" --timeout 900 --stats > "$out/armless.log"
grep -q ARMLESS-USERSPACE-OK "$out/armless.log"
sed -n '/^== userspace ==/,/^ARMLESS-USERSPACE-OK/p' "$out/armless.log" \
    | grep -v -e '^== ' -e '^ARMLESS-USERSPACE-OK' -e '^dc' | tr -d '\r' > "$out/dut.txt"
diff -u "$out/ref.txt" "$out/dut.txt"
echo "userspace test passed: $(wc -l < "$out/dut.txt") lines identical to qemu-aarch64"
