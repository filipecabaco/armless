#!/bin/sh
# Build the boot-test kernel and initramfs.
#
#   tests/linux/build.sh LINUX_SRC_DIR [OUT_DIR]
#
# Needs: aarch64-linux-gnu-gcc (or set CROSS_COMPILE), make, flex, bison, bc, python3.
# Produces OUT_DIR/Image and OUT_DIR/initrd.cpio.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src=${1:?usage: build.sh LINUX_SRC_DIR [OUT_DIR]}
out=${2:-$here/../../build-linux}
cross=${CROSS_COMPILE:-aarch64-linux-gnu-}
mkdir -p "$out"
out=$(realpath "$out")

cp "$here/armless_defconfig" "$src/arch/arm64/configs/armless_defconfig"
make -C "$src" ARCH=arm64 CROSS_COMPILE="$cross" armless_defconfig
make -C "$src" ARCH=arm64 CROSS_COMPILE="$cross" -j"$(nproc)" Image
cp "$src/arch/arm64/boot/Image" "$out/Image"

"${cross}gcc" -static -nostdlib -ffreestanding -fno-stack-protector -mgeneral-regs-only -O2 \
    -o "$out/init" "$here/init.c"
python3 "$here/mkcpio.py" "$out/initrd.cpio" "init=$out/init"
echo "built $out/Image and $out/initrd.cpio"
