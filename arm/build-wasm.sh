#!/bin/sh
# Build armless.wasm: the Rust core plus v86's zstd decoder (for .zst
# snapshots and 9p blobs). Output: ../build/armless.wasm
#
# Uses the wasm32-unknown-unknown std from rustup if installed; otherwise set
# BUILD_STD=1 to build std from source (needs the rust-src component).
#
# SMP=1 builds the threaded variant instead, ../build/armless-smp.wasm: shared
# imported memory and atomics, for several vCPUs in Web Workers. That always
# builds std from source (with atomics), so it needs the rust-src component.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
mkdir -p "$root/build"

if [ "${SMP:-0}" = 1 ]; then
    name=armless-smp
    cflags="-matomics -mbulk-memory"
else
    name=armless
    cflags=""
fi

clang -c -Wall --target=wasm32 -O3 -nostdlib -fvisibility=hidden $cflags \
    -ffunction-sections -fdata-sections -DZSTDLIB_VISIBILITY="" \
    -o "$root/build/$name-zstd.o" "$root/lib/zstd/zstddeclib.c"

cd "$here"
if [ "${SMP:-0}" = 1 ]; then
    # Separate target dir: std is rebuilt with different features.
    export CARGO_TARGET_DIR="$root/build/smp-target"
    export RUSTC_BOOTSTRAP=1
    export RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals"
    cargo rustc --release --lib --target wasm32-unknown-unknown -Z build-std=std,panic_abort -- \
        -C link-args="--import-table --shared-memory --import-memory --max-memory=4294967296" \
        -C link-args="--export=__stack_pointer --export=__wasm_init_tls --export=__tls_size --export=__tls_align" \
        -C link-args="$root/build/$name-zstd.o"
    cp "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/armless.wasm" "$root/build/$name.wasm"
else
    extra=""
    if [ "${BUILD_STD:-0}" = 1 ]; then
        export RUSTC_BOOTSTRAP=1
        extra="-Z build-std=std,panic_abort"
    fi
    cargo rustc --release --lib --target wasm32-unknown-unknown $extra -- \
        -C target-feature=+bulk-memory \
        -C link-args="--import-table" \
        -C link-args="$root/build/$name-zstd.o"
    cp "$root/build/wasm32-unknown-unknown/release/armless.wasm" "$root/build/$name.wasm"
fi
ls -l "$root/build/$name.wasm"
