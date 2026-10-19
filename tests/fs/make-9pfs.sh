#!/bin/sh
# Build a v86 9p filesystem (fs.json + zstd blobs, as snowglobe does) with a
# busybox root for tests/vm.rs and tests/js/flow.mjs.
#
#   tests/fs/make-9pfs.sh BUSYBOX_BINARY KERNEL OUT_DIR
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
# Data for the network and 9p tests: an 8 MiB incompressible file (in chunks) and many
# small ones.
mkdir -p "$tree/data/small"
python3 - "$tree/data" <<'PY'
import hashlib, os, sys
d = sys.argv[1]
with open(os.path.join(d, "big.bin"), "wb") as f:
    for i in range(8 * 1024 * 1024 // 32):
        f.write(hashlib.sha256(i.to_bytes(8, "little")).digest())
for i in range(300):
    with open(os.path.join(d, "small", "f%03d" % i), "w") as f:
        f.write("small file %d\n" % i * (i % 7 + 1))
PY

rm -rf "$out/blobs"
mkdir -p "$out/blobs"
python3 "$here/fs2json.py" --zstd --out "$out/fs.json" "$tree" >/dev/null
python3 "$here/copy-to-sha256.py" --zstd "$tree" "$out/blobs" >/dev/null
cp "$kernel" "$out/vmlinuz"
rm -rf "$tree"
echo "built $out (fs.json, blobs/, vmlinuz)"
