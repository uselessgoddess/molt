#!/usr/bin/env bash
# Checks that the loom suite can actually fail.
#
# A model-checking suite that passes because it explores nothing looks exactly
# like one that passes because the code is correct. This removes the wake from
# CompletionSlab::complete — a textbook lost wakeup — and asserts loom notices,
# then restores the file.
#
# Usage: experiments/loom-detects-lost-wakeup.sh
set -euo pipefail

cd "$(dirname "$0")/.."
target=crates/molt-core/src/completion.rs
backup=$(mktemp)
trap 'cp "$backup" "$target"; rm -f "$backup"' EXIT

cp "$target" "$backup"
perl -0pi -e 's/^(\s+)slot\.waker\.wake\(\);$/$1\/\/ removed by loom-detects-lost-wakeup.sh/m' "$target"
grep -q "removed by loom-detects-lost-wakeup" "$target" || {
    echo "FAIL: could not inject the bug; has complete() changed?" >&2
    exit 1
}

echo "== running loom against a deliberately broken complete() =="
if LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" \
    cargo test --package molt-core --profile loom --lib 2>&1 | tee /tmp/loom-injected.log | tail -20; then
    echo "FAIL: loom passed a lost wakeup. The suite is not exploring." >&2
    exit 1
fi

grep -q "parked without a wake" /tmp/loom-injected.log || {
    echo "FAIL: loom failed, but not on the lost-wakeup assertion." >&2
    exit 1
}

echo "== OK: loom caught the lost wakeup =="
