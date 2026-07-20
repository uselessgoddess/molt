#!/usr/bin/env bash
# Proves the image audit fails when `.text` is mapped writable.
#
# A check that only ever passes is indistinguishable from no check. This makes
# the RISC-V boot mapping grant `.text` write rights — the state the old RWX
# gigapage left the kernel in — and asserts the smoke run fails instead of
# printing MOLT_WX_OK.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
paging="$root/crates/platforms/riscv/src/paging.rs"
backup=$(mktemp -t molt-paging-XXXXXX.rs)
cp "$paging" "$backup"
trap 'cp "$backup" "$paging"; rm -f "$backup"' EXIT

# The single line that decides `.text`'s rights.
sed -i 's/bound!(__text_end), PTE_R | PTE_X)/bound!(__text_end), PTE_R | PTE_W | PTE_X)/' "$paging"
if ! diff -q "$backup" "$paging" >/dev/null; then
    echo "injected: .text mapped writable"
else
    echo "the .text mapping line moved; update the sed pattern" >&2
    exit 1
fi

log=$(mktemp -t molt-wx-smoke-XXXXXX.log)
set +e
(cd "$root" && cargo smoke riscv64) >"$log" 2>&1
status=$?
set -e

if [ "$status" -eq 0 ]; then
    echo "FAIL: the smoke run passed with a writable .text"
    rm -f "$log"
    exit 1
fi

echo "PASS: smoke exited $status"
grep -a "MOLT_WX_OK\|MOLT_MAPPING_OK\|MOLT_PANIC\|marker" "$log" | tail -5 || true
rm -f "$log"
