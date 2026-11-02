#!/bin/sh
# Build the bare-metal test guest with clang + lld (no cross toolchain needed).
# LD and OBJCOPY name the linker and objcopy (rustc's own work: tests/guest.rs uses them when
# ld.lld and llvm-objcopy are not installed).
set -eu
LD=${LD:-ld.lld}
OBJCOPY=${OBJCOPY:-llvm-objcopy}
cd "$(dirname "$0")"
OUT=${OUT:-../build-guest}
mkdir -p "$OUT"
CFLAGS="--target=aarch64-none-elf -march=armv8-a -mgeneral-regs-only -mno-outline-atomics
        -ffreestanding -fno-builtin -fno-stack-protector -fno-pic -O2 -Wall -Wextra -Werror"
clang $CFLAGS -c start.S -o "$OUT/start.o"
clang $CFLAGS -c kernel.c -o "$OUT/kernel.o"
"$LD" -T link.ld -o "$OUT/tests.elf" "$OUT/start.o" "$OUT/kernel.o"
"$OBJCOPY" -O binary "$OUT/tests.elf" "$OUT/tests.bin"
# The benchmark (bench/run.exs): its C may use SIMD&FP, which start.S then enables first
BFLAGS="--target=aarch64-none-elf -march=armv8-a -mno-outline-atomics
        -ffreestanding -fno-builtin -fno-stack-protector -fno-pic -O2 -Wall -Wextra -Werror"
clang $BFLAGS -DENABLE_FP -c start.S -o "$OUT/bench-start.o"
clang $BFLAGS -c bench.c -o "$OUT/bench.o"
"$LD" -T link.ld -o "$OUT/bench.elf" "$OUT/bench-start.o" "$OUT/bench.o"
"$OBJCOPY" -O binary "$OUT/bench.elf" "$OUT/bench.bin"
echo "$OUT/tests.bin"
