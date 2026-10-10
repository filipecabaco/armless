#!/bin/sh
# Build a v86 9p filesystem (fs.json + zstd blobs, as snowglobe does) with a
# busybox root for tests/arm/snowglobe.mjs.
#
#   tests/arm/make-9pfs.sh BUSYBOX_BINARY KERNEL OUT_DIR
#
# Needs: python3 with the zstandard module, qemu-aarch64 (to list applets).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
busybox=$(realpath "${1:?usage: make-9pfs.sh BUSYBOX KERNEL OUT_DIR}")
kernel=$(realpath "${2:?usage: make-9pfs.sh BUSYBOX KERNEL OUT_DIR}")
out=${3:?usage: make-9pfs.sh BUSYBOX KERNEL OUT_DIR}

tree=$(mktemp -d)
mkdir -p "$tree/bin" "$tree/proc" "$tree/sys" "$tree/dev" "$tree/tmp" "$tree/etc" "$tree/root"
cp "$busybox" "$tree/bin/busybox"
for applet in $(qemu-aarch64 "$busybox" --list); do
    [ "$applet" = busybox ] || ln -sf busybox "$tree/bin/$applet"
done
cp "$here/rootfs/init" "$tree/init"
chmod +x "$tree/init"
echo "hello from 9p" > "$tree/etc/motd"

rm -rf "$out/blobs"
mkdir -p "$out/blobs"
python3 "$root/tools/fs2json.py" --zstd --out "$out/fs.json" "$tree" >/dev/null
python3 "$root/tools/copy-to-sha256.py" --zstd "$tree" "$out/blobs" >/dev/null
cp "$kernel" "$out/vmlinuz"
rm -rf "$tree"
echo "built $out (fs.json, blobs/, vmlinuz)"
