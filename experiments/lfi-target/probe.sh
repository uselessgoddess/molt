#!/usr/bin/env bash
# Does stock rustc honour the register reservations LFI-RISCV requires?
set -euo pipefail
cd "$(dirname "$0")"

target=riscv64gc-unknown-none-elf
common=(--edition 2024 --target "$target" --crate-type lib -O --emit=asm)

rustc "${common[@]}" -o stock.s probe.rs
rustc "${common[@]}" -C target-feature=+zba,+reserve-x18,+reserve-x21 -o reserved.s probe.rs

# s2 is x18, s5 is x21: the two registers the LFI-RISCV verifier reserves.
echo "--- stock: s2(x18)/s5(x21) uses ---"
grep -cE '\bs2\b|\bs5\b' stock.s || true
echo "--- reserved: s2(x18)/s5(x21) uses ---"
grep -cE '\bs2\b|\bs5\b' reserved.s || true
echo "--- zba in reserved (add.uw / sh1add) ---"
grep -cE 'add\.uw|sh[123]add' reserved.s || true
