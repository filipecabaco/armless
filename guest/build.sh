#!/bin/sh
# Build the bare-metal test guest with clang + lld (no cross toolchain needed).
set -eu
cd "$(dirname "$0")"
OUT=${OUT:-../build-guest}
mkdir -p "$OUT"
CFLAGS="--target=aarch64-none-elf -march=armv8-a -mgeneral-regs-only -mno-outline-atomics
        -ffreestanding -fno-builtin -fno-stack-protector -fno-pic -O2 -Wall -Wextra -Werror"
clang $CFLAGS -c start.S -o "$OUT/start.o"
clang $CFLAGS -c kernel.c -o "$OUT/kernel.o"
ld.lld -T link.ld -o "$OUT/tests.elf" "$OUT/start.o" "$OUT/kernel.o"
llvm-objcopy -O binary "$OUT/tests.elf" "$OUT/tests.bin"
echo "$OUT/tests.bin"
