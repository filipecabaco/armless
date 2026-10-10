#!/bin/sh
# Boot the test kernel on armless-run and check that init reached userspace.
#
#   tests/linux/boot.sh [OUT_DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath "${1:-$here/../../build-linux}")
cd "$here/../.."
cargo build --release --bin armless-run
log=$(mktemp)
build/release/armless-run --kernel "$out/Image" --initrd "$out/initrd.cpio" \
    --append "console=ttyS0 earlycon=uart8250,mmio,0xf0100000 rdinit=/init" \
    --timeout 600 --stats | tee "$log"
grep -q "ARMLESS-BOOT-OK" "$log"
echo "boot test passed"
