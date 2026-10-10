#!/bin/sh
# Build armless.wasm: the Rust core plus v86's zstd decoder (for .zst
# snapshots and 9p blobs). Output: ../build/armless.wasm
#
# Uses the wasm32-unknown-unknown std from rustup if installed; otherwise set
# BUILD_STD=1 to build std from source (needs the rust-src component).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
mkdir -p "$root/build"

clang -c -Wall --target=wasm32 -O3 -nostdlib -fvisibility=hidden \
    -ffunction-sections -fdata-sections -DZSTDLIB_VISIBILITY="" \
    -o "$root/build/armless-zstd.o" "$root/lib/zstd/zstddeclib.c"

cd "$here"
extra=""
if [ "${BUILD_STD:-0}" = 1 ]; then
    export RUSTC_BOOTSTRAP=1
    extra="-Z build-std=std,panic_abort"
fi
cargo rustc --release --lib --target wasm32-unknown-unknown $extra -- \
    -C target-feature=+bulk-memory \
    -C link-args="$root/build/armless-zstd.o"
cp "$root/build/wasm32-unknown-unknown/release/armless.wasm" "$root/build/armless.wasm"
ls -l "$root/build/armless.wasm"
