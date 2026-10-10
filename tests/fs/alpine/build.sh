#!/bin/sh
# Build the arm64 Alpine test image and convert it to a v86 9p filesystem.
#   tests/fs/alpine/build.sh OUT_DIR      (needs docker buildx; on an x86 host, arm64 emulation)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out=$(mkdir -p "$1" && cd "$1" && pwd)
docker buildx build --platform linux/arm64 --load -t armless-alpine-test "$here"
cid=$(docker create --platform linux/arm64 armless-alpine-test)
rm -rf "$out/rootfs" "$out/blobs"
mkdir -p "$out/rootfs" "$out/blobs"
docker export "$cid" | sudo tar -x -C "$out/rootfs"
docker rm "$cid" >/dev/null
sudo rm -f "$out/rootfs/.dockerenv"
sudo python3 "$here/../fs2json.py" --zstd --out "$out/fs.json" "$out/rootfs" >/dev/null
sudo python3 "$here/../copy-to-sha256.py" --zstd "$out/rootfs" "$out/blobs" >/dev/null
sudo chown -R "$(id -u):$(id -g)" "$out/fs.json" "$out/blobs"
ls "$out/rootfs/boot"
du -sh "$out/blobs"
