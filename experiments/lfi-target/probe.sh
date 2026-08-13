#!/usr/bin/env bash
#
# Shows that stock rustc holds back the registers LFI-RISCV reserves.
#
# `docs/userspace.md` decides against a compiler fork on the strength of one
# claim: `-C target-feature=+reserve-x18,+reserve-x21` is enough, so Molt's
# sandboxes can be built by the pinned nightly rather than by a patched rustc.
# That is a claim about a codegen backend, not about a document, so it is read
# off the machine code the compiler actually emitted.
#
# The same source is compiled twice for `riscv64gc-unknown-none-elf` and the
# uses of `s2`(`x18`) and `s5`(`x21`) counted in each. Stock has to use them —
# a probe the allocator never pushes that far would report zero for the wrong
# reason — and the reserved build has to use neither. `add.uw` has to survive
# too, since it is the instruction the 4 GiB window is made of.
#
# Usage: experiments/lfi-target/probe.sh

set -euo pipefail

cd "$(dirname "$0")"

out=../../target/lfi-target
mkdir -p "$out"

objdump=$(find "$(rustc --print sysroot)" -name 'llvm-objdump' -type f | head -1)
if [[ -z "$objdump" ]]; then
    echo "skipped: llvm-tools-preview is not installed, so there is nothing to read"
    exit 0
fi

compile() {
    rustc --edition 2024 --crate-type lib --target riscv64gc-unknown-none-elf \
        -O --emit obj -o "$out/$1.o" "${@:2}" probe.rs
}

# Every mention, not every write: a register the reservation missed shows up as
# an operand somewhere, so the count is over instructions naming either one.
saved() {
    "$objdump" -d --no-show-raw-insn "$out/$1.o" | grep -coE '\bs2\b|\bs5\b' || true
}

compile stock 2>&1 | grep -v '^$' || true
compile reserved -C target-feature=+zba,+reserve-x18,+reserve-x21 2>&1 | sed 's/^/    /'

echo "--- stock: instructions naming s2(x18)/s5(x21) ---"
stock=$(saved stock)
echo "$stock"
echo "--- reserved: instructions naming s2(x18)/s5(x21) ---"
reserved=$(saved reserved)
echo "$reserved"
echo "--- zba in reserved (add.uw / sh1add) ---"
zba=$("$objdump" -d --no-show-raw-insn "$out/reserved.o" | grep -cE '\b(add\.uw|sh[123]add)\b' || true)
echo "$zba"

if ((stock == 0)); then
    echo "FAILED: the stock build used neither register, so the probe proves nothing"
    exit 1
fi
if ((reserved != 0)); then
    echo "FAILED: $reserved uses survived the reservation"
    exit 1
fi
if ((zba == 0)); then
    echo "FAILED: no Zba instruction, so the 4 GiB window has nothing to be made of"
    exit 1
fi

echo
echo "ok: $stock stock, none reserved, $zba Zba instructions kept"
