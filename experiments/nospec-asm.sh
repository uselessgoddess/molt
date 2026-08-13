#!/usr/bin/env bash
#
# Shows that the Spectre-v1 mask survives the optimizer.
#
# The unit tests in `molt_abi::nospec` prove the mask is right. They cannot
# prove it is still there after LLVM has had the source, which is the failure
# mode this mitigation actually has: on the path where the bounds check passed
# the mask is provably all ones, so an optimizer that can see through it will
# delete the `and` and leave a bounds check that is correct and useless.
#
# So this reads the machine code. `Call::parse` is the only public entry the
# region check has, and in its body the mask has to come out of an `#APP`
# barrier (`black_box`) and be `and`ed into the offset and the length. Grep for
# exactly that, print the window either way, and say which it was.
#
# Usage: experiments/nospec-asm.sh   (x86_64 hosts; the shape is the same on
# RISC-V, the mnemonics are not)

set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "$(uname -m)" != "x86_64" ]]; then
    echo "skipped: this reads x86_64 mnemonics, and the host is $(uname -m)"
    exit 0
fi

cargo rustc -p molt-abi --release --lib -- --emit asm >/dev/null
asm=$(ls -t target/release/deps/molt_abi-*.s | head -1)

symbol=$(grep -o '^[^ :]*Call5parse:' "$asm" | head -1 | tr -d ':')
body=$(awk -v sym="$symbol:" '$0 == sym {inside = 1; next} inside && /\.Lfunc_end/ {exit} inside' "$asm")

echo "$body" > target/nospec-asm.txt
echo "the body of $symbol is in target/nospec-asm.txt"
echo

barriers=$(grep -c '#APP' <<< "$body" || true)
masks=$(grep -cE '^[[:space:]]+and[a-z]*[[:space:]]' <<< "$body" || true)

grep -nE '#APP|#NO_APP|^[[:space:]]+(and|neg|seta|sbb)[a-z]*[[:space:]]' <<< "$body" | head -20
echo

if ((barriers >= 2 && masks >= 2)); then
    echo "ok: $barriers barriers and $masks masked values, so the mask outlived the optimizer"
else
    echo "FAILED: $barriers barriers and $masks masked values — the mask was folded away"
    exit 1
fi
