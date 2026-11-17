#!/bin/sh
# Free page reporting on the test kernel (armless_defconfig has CONFIG_VIRTIO_BALLOON and
# CONFIG_PAGE_REPORTING): an init writes and frees 200 MiB, and a snapshot taken after the kernel
# reported it must be at least 150 MB smaller with --free-page-reporting than without.
#
#   tests/linux/free-pages.sh [OUT_DIR]
#
# Needs OUT_DIR/Image (tests/linux/build.sh). CC builds the init (default: the cross gcc).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath "${1:-$here/../../build-linux}")
cc=${CC:-${CROSS_COMPILE:-aarch64-linux-gnu-}gcc}
cd "$here/../.."
$cc -static -nostdlib -ffreestanding -fno-stack-protector -mgeneral-regs-only -O2 \
    -o "$out/free-pages" "$here/free-pages.c"
python3 "$here/mkcpio.py" "$out/free-pages.cpio" "init=$out/free-pages"
cargo build --release --bin armless-run
size() {
    build/release/armless-run --kernel "$out/Image" --initrd "$out/free-pages.cpio" \
        --append "console=ttyS0 rdinit=/init" -m 512 --timeout 300 "$@" \
        --save-after ARMLESS-FREED-AND-WAITED --save "$out/free-pages.state" >&2
    wc -c < "$out/free-pages.state"
}
without=$(size)
with=$(size --free-page-reporting)
echo "snapshot without free page reporting: $without bytes, with: $with bytes"
if [ $((without - with)) -lt $((150 << 20)) ]; then
    echo "free page reporting left the freed memory in the snapshot" >&2
    exit 1
fi
echo "free page reporting test passed"
