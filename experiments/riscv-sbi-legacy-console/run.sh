#!/usr/bin/env bash
# Proves the legacy console fallback still boots the kernel.
#
# QEMU's bundled OpenSBI implements DBCN, so every normal smoke run takes the
# debug-console path and the fallback is never exercised. This forces the probe
# to fail, then asserts the boot log says `Legacy` and the run still passes —
# the firmware-without-DBCN case, which no CI host reproduces on its own.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
sbi="$root/crates/platforms/riscv/src/sbi.rs"
backup=$(mktemp -t molt-sbi-XXXXXX.rs)
cp "$sbi" "$backup"
trap 'cp "$backup" "$sbi"; rm -f "$backup"' EXIT

# The single line that decides which console the port settles on.
sed -i 's/^    probe(EXT_DEBUG_CONSOLE)$/    false \&\& probe(EXT_DEBUG_CONSOLE)/' "$sbi"
if diff -q "$backup" "$sbi" >/dev/null; then
    echo "the probe call moved; update the sed pattern" >&2
    exit 1
fi
echo "injected: DBCN probe forced to fail"

log=$(mktemp -t molt-legacy-smoke-XXXXXX.log)
set +e
(cd "$root" && cargo smoke riscv64) >"$log" 2>&1
status=$?
set -e

if [ "$status" -ne 0 ]; then
    echo "FAIL: the smoke run failed on the legacy console"
    grep -a "MOLT_\|marker" "$log" | tail -10 || true
    rm -f "$log"
    exit 1
fi

if ! grep -aq "MOLT_SBI_CONSOLE: Legacy" "$log"; then
    echo "FAIL: the boot log did not report the legacy console"
    grep -a "MOLT_SBI_CONSOLE" "$log" || true
    rm -f "$log"
    exit 1
fi

echo "PASS: smoke exited 0 on the legacy console"
grep -a "MOLT_SBI_CONSOLE\|MOLT_BOOT_OK" "$log" | head -5 || true
rm -f "$log"
