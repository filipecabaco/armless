#!/bin/sh
# Differential test: run random instructions on qemu-aarch64 and armless, diff.
#
#   tests/difftest/run.sh [CLASSES] [COUNT] [SEED]      e.g. run.sh int 5000 7
#
# Needs: python3, aarch64-linux-gnu-gcc, qemu-aarch64, clang, ld.lld, llvm-objcopy.
# DUT=jit runs armless.wasm under Node with every entry JIT-compiled (DUT=wasm:
# wasm interpreter only); the default is the native armless-run.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
classes=${1:-int}
count=${2:-2000}
seed=${3:-1}
out=${OUT:-$here/../../build-difftest}
mkdir -p "$out"
cd "$here"

simd=""
case "$classes" in *simd*|*fp*|*ldst*) simd="-DWITH_SIMD" ;; esac
case "$classes" in *ldst*) simd="$simd -DLDST" ;; esac

python3 gen.py --classes "$classes" --count "$count" --seed "$seed" > "$out/cases.S"

# Reference: Linux user mode under QEMU (Cortex-A53 = plain ARMv8.0).
# C code avoids FP/SIMD registers; the assembly needs them for the harness.
GCC=aarch64-linux-gnu-gcc
GCFLAGS="-ffreestanding -fno-stack-protector -O1 $simd"
$GCC $GCFLAGS -mgeneral-regs-only -c main_linux.c -o "$out/main_linux.o"
$GCC $GCFLAGS -mgeneral-regs-only -c report.c -o "$out/report_ref.o"
$GCC $GCFLAGS -c harness.S -o "$out/harness_ref.o"
$GCC $GCFLAGS -c "$out/cases.S" -o "$out/cases_ref.o"
$GCC -static -nostdlib -o "$out/ref.elf" "$out/main_linux.o" "$out/report_ref.o" "$out/harness_ref.o" "$out/cases_ref.o"
qemu-aarch64 -cpu cortex-a53 "$out/ref.elf" > "$out/ref.txt"

# armless: bare metal
CFLAGS="--target=aarch64-none-elf -march=armv8-a -ffreestanding -fno-builtin -fno-stack-protector -fno-pic -O1 $simd"
clang $CFLAGS -c main_bare.S -o "$out/main_bare.o"
clang $CFLAGS -mgeneral-regs-only -c report.c -o "$out/report.o"
clang $CFLAGS -c harness.S -o "$out/harness.o"
clang $CFLAGS -c "$out/cases.S" -o "$out/cases.o"
ld.lld -T ../../guest/link.ld -o "$out/dut.elf" "$out/main_bare.o" "$out/report.o" "$out/harness.o" "$out/cases.o"
llvm-objcopy -O binary "$out/dut.elf" "$out/dut.bin"
if [ "${DUT:-native}" = jit ]; then
    # armless.wasm in Node, every block JIT-compiled before it first runs
    node ../../../tests/arm/run-bare.mjs "$out/dut.bin" -m 64 --jit-threshold 1 --jit-sync > "$out/dut.txt" || true
elif [ "${DUT:-native}" = wasm ]; then
    node ../../../tests/arm/run-bare.mjs "$out/dut.bin" -m 64 --no-jit > "$out/dut.txt" || true
else
    (cd ../.. && cargo build --release -q --bin armless-run)
    ../../../build/release/armless-run --raw "$out/dut.bin" -m 64 > "$out/dut.txt" || true
fi

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
