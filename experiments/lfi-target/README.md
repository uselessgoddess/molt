# Can stock rustc emit LFI-compatible RISC-V code?

The LFI-RISCV verifier reserves `x21` for the sandbox base and permits only
`add.uw` to write `x18`, `ra`, and `sp`
([`riscv/verifier.tex`](https://github.com/lfi-project/lfi-specification)), and
`lfi-rewrite` "expects that the assembly does not make use of any reserved
registers". The LFI toolchain gets that from a compiler fork. The question this
experiment answers is whether Molt needs one.

## Run

```
./probe.sh
```

It compiles the same crate twice for `riscv64gc-unknown-none-elf` — once
stock, once with `-C target-feature=+zba,+reserve-x18,+reserve-x21` — and counts
uses of `s2` (`x18`) and `s5` (`x21`) in the emitted assembly.

## Result, on `nightly-2026-05-24`

```
--- stock: s2(x18)/s5(x21) uses ---
4
--- reserved: s2(x18)/s5(x21) uses ---
0
--- zba in reserved (add.uw / sh1add) ---
2
```

So the reservations hold, and Zba — which LFI-RISCV requires, because `add.uw`
is what implements the sandbox base — is a stock target feature.

Two caveats the run reports itself:

```
warning: unknown and unstable feature specified for `-Ctarget-feature`: `reserve-x18`
  = note: it is still passed through to the codegen backend, but use of this
    feature might be unsound and the behavior of this feature can change
```

The features are LLVM's, passed through rather than blessed by rustc. And a
reservation only covers code rustc compiles: `core` must be rebuilt with the
same features, which is `-Z build-std` and the reason the target has to be a
JSON spec rather than a set of flags.

There is no equivalent for x86-64 — `rustc --print target-features --target
x86_64-unknown-none` lists no `reserve-*` or `fixed-*` feature, so `%r14` and
`%r11` cannot be held back the same way.

See [`docs/userspace.md`](../../docs/userspace.md) for what follows from that.
