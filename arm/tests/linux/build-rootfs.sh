#!/bin/sh
# Build a busybox + glibc test initramfs for the userspace test.
#
#   tests/linux/build-rootfs.sh BUSYBOX_SRC [OUT_DIR]
#
# Needs: aarch64-linux-gnu-gcc (static glibc), make, python3.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bb=${1:?usage: build-rootfs.sh BUSYBOX_SRC [OUT_DIR]}
out=${2:-$here/../../build-linux}
cross=${CROSS_COMPILE:-aarch64-linux-gnu-}
mkdir -p "$out"

make -C "$bb" ARCH=arm64 CROSS_COMPILE="$cross" defconfig >/dev/null
sed -i \
    -e 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' \
    -e 's/^CONFIG_TC=y/# CONFIG_TC is not set/' \
    -e 's/^CONFIG_SHA1_HWACCEL=y/# CONFIG_SHA1_HWACCEL is not set/' \
    -e 's/^CONFIG_SHA256_HWACCEL=y/# CONFIG_SHA256_HWACCEL is not set/' \
    -e 's/^# CONFIG_FEATURE_PREFER_APPLETS is not set/CONFIG_FEATURE_PREFER_APPLETS=y/' \
    -e 's/^# CONFIG_FEATURE_SH_STANDALONE is not set/CONFIG_FEATURE_SH_STANDALONE=y/' \
    "$bb/.config"
make -C "$bb" ARCH=arm64 CROSS_COMPILE="$cross" -j"$(nproc)" busybox >/dev/null
cp "$bb/busybox" "$out/busybox"

"${cross}gcc" -O2 -static -pthread -o "$out/userspace" "$here/userspace.c" -lm

python3 "$here/mkcpio.py" "$out/rootfs.cpio" \
    "init=$here/rootfs/init" "tests.sh=$here/rootfs/tests.sh" \
    "userspace=$out/userspace" "bin/busybox=$out/busybox" proc/ sys/ tmp/
echo "built $out/rootfs.cpio"
