#!/bin/sh
# Builds the probes armless-exec is measured with: probe-go (static linux/arm64 Go: goroutines,
# futexes, the netpoller, a GC) and hello (one write and exit_group, no libc). Needs Go, and clang
# with an lld (rustup's rust-lld under gcc-ld/ will do: LLD_DIR=...).
set -eu
cd "$(dirname "$0")"
CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags='-s -w' -o probe-go main.go
lld=${LLD_DIR:-$(ls -d "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/gcc-ld 2>/dev/null | head -1)}
clang --target=aarch64-linux-gnu -nostdlib -static -ffreestanding -fno-stack-protector -O2 \
    ${lld:+-B"$lld"} -fuse-ld=lld -o hello hello.c
