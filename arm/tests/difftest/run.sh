#!/bin/sh
# Differential test: run random instructions on qemu-aarch64 and armless, diff.
#
#   tests/difftest/run.sh [CLASSES] [COUNT] [SEED]      e.g. run.sh int 5000 7
#
# Needs: python3, aarch64-linux-gnu-gcc, qemu-aarch64, clang, ld.lld, llvm-objcopy.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
classes=${1:-int}
count=${2:-2000}
seed=${3:-1}
out=${OUT:-$here/../../build-difftest}
mkdir -p "$out"
cd "$here"

simd=""
case "$classes" in *simd*|*fp*) simd="-DWITH_SIMD" ;; esac

python3 gen.py --classes "$classes" --count "$count" --seed "$seed" > "$out/cases.S"

# Reference: Linux user mode under QEMU (Cortex-A53 = plain ARMv8.0)
aarch64-linux-gnu-gcc -static -nostdlib -ffreestanding -fno-stack-protector -mgeneral-regs-only -O1 \
    $simd -o "$out/ref.elf" main_linux.c report.c harness.S "$out/cases.S"
qemu-aarch64 -cpu cortex-a53 "$out/ref.elf" > "$out/ref.txt"

# armless: bare metal
CFLAGS="--target=aarch64-none-elf -march=armv8-a -mgeneral-regs-only -ffreestanding -fno-builtin
        -fno-stack-protector -fno-pic -O1 $simd"
clang $CFLAGS -c main_bare.S -o "$out/main_bare.o"
clang $CFLAGS -c report.c -o "$out/report.o"
clang $CFLAGS -c harness.S -o "$out/harness.o"
clang $CFLAGS -c "$out/cases.S" -o "$out/cases.o"
ld.lld -T ../../guest/link.ld -o "$out/dut.elf" "$out/main_bare.o" "$out/report.o" "$out/harness.o" "$out/cases.o"
llvm-objcopy -O binary "$out/dut.elf" "$out/dut.bin"
(cd ../.. && cargo build --release -q --bin armless-run)
../../../build/release/armless-run --raw "$out/dut.bin" -m 64 > "$out/dut.txt" || true

total=$(wc -l < "$out/ref.txt")
undef=$(grep -c UNDEF "$out/ref.txt" || true)
if diff -u "$out/ref.txt" "$out/dut.txt" > "$out/diff.txt"; then
    echo "difftest $classes seed=$seed: all $total cases match ($undef undefined in both)"
else
    bad=$(grep -c '^-[0-9]' "$out/diff.txt" || true)
    echo "difftest $classes seed=$seed: $bad of $total cases differ (see $out/diff.txt)"
    head -40 "$out/diff.txt"
    exit 1
fi
