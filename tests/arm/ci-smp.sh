#!/bin/sh
# CI: the arm64 machine with two CPUs through the V86 API (vCPU workers).
# Each check's log tail goes into a GitHub annotation if it fails.
#
#   tests/arm/ci-smp.sh BUILD_LINUX_DIR
set -u
here=$(cd "$(dirname "$0")" && pwd)
dir=$(realpath "${1:?usage: ci-smp.sh BUILD_LINUX_DIR}")
export ARMLESS_CPUS=2
status=0

fail() {
    echo "::error title=$1::$(tail -c 1500 "$2" | tr -d '\r%' | tr '\n' ';')"
    status=1
}

node "$here/boot.mjs" --cmdline "console=ttyS0 rdinit=/init quiet" \
    --expect ARMLESS-SMP-DONE "$dir/Image" "$dir/smp.cpio" > "$dir/v86-smp.log" 2>&1 \
    || fail "V86 2 CPUs: SMP stress did not finish" "$dir/v86-smp.log"
grep -E "^smp:" "$dir/v86-smp.log"
sed -n '/^== smp ==/,/^ARMLESS-SMP-DONE/p' "$dir/v86-smp.log" \
    | grep -v -e '^== ' -e '^ARMLESS-SMP-DONE' -e '^smp:' -e 'SERIAL:' | tr -d '\r' > "$dir/v86-smp-dut.txt"
diff -u "$dir/smp-ref.txt" "$dir/v86-smp-dut.txt" > "$dir/v86-smp-diff.txt" \
    || fail "V86 2 CPUs: SMP stress output differs from qemu-aarch64" "$dir/v86-smp-diff.txt"

node "$here/snowglobe.mjs" "$dir/9pfs" > "$dir/v86-smp-snowglobe.log" 2>&1 \
    || fail "V86 2 CPUs: snowglobe flow" "$dir/v86-smp-snowglobe.log"
grep "snowglobe.mjs" "$dir/v86-smp-snowglobe.log"

NETFS_ONLY=net node "$here/netfs.mjs" "$dir/9pfs" > "$dir/v86-smp-netfs.log" 2>&1 \
    || fail "V86 2 CPUs: network stress" "$dir/v86-smp-netfs.log"
grep -E "^(ok|FAIL)" "$dir/v86-smp-netfs.log"
exit $status
