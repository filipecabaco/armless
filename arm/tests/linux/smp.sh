#!/bin/sh
# SMP test: boot Linux with several CPUs and run tests/linux/smp.c, a pthread
# stress test (atomics, futexes, lock-free stack, mmap/TLB churn). Its output
# must match qemu-aarch64's.
#
#   tests/linux/smp.sh [OUT_DIR] [CPUS]
#
# Needs OUT_DIR/Image and OUT_DIR/busybox (tests/linux/build.sh, build-rootfs.sh).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath "${1:-$here/../../build-linux}")
cpus=${2:-4}
cross=${CROSS_COMPILE:-aarch64-linux-gnu-}
cd "$here/../.."
cargo build --release -q --bin armless-run

"${cross}gcc" -O2 -static -pthread -o "$out/smp" "$here/smp.c"
cat > "$out/smp-init" <<'EOF'
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc proc /proc
echo "== smp =="
/smp
echo ARMLESS-SMP-DONE
poweroff -f
EOF
python3 "$here/mkcpio.py" "$out/smp.cpio" \
    "init=$out/smp-init" "smp=$out/smp" "bin/busybox=$out/busybox" proc/ sys/ tmp/

qemu-aarch64 -cpu cortex-a53 "$out/smp" > "$out/smp-ref.txt" 2>/dev/null

../build/release/armless-run --smp "$cpus" -m 512 --kernel "$out/Image" --initrd "$out/smp.cpio" \
    --append "console=ttyS0 rdinit=/init quiet" --timeout 900 --stats > "$out/smp.log" 2>&1
grep -E "^smp:|SMP: Total|armless-run" "$out/smp.log" || true
sed -n '/^== smp ==/,/^ARMLESS-SMP-DONE/p' "$out/smp.log" \
    | grep -v -e '^== ' -e '^ARMLESS-SMP-DONE' -e '^smp:' | tr -d '\r' > "$out/smp-dut.txt"
diff -u "$out/smp-ref.txt" "$out/smp-dut.txt"
echo "smp test passed on $cpus CPUs"
