# Does stock rustc hold back the registers LFI reserves?

[`docs/userspace.md`](../../docs/userspace.md) decides against forking the
compiler. The whole decision rests on one thing being true of the pinned
`nightly-2026-05-24`: that `-C target-feature=+reserve-x18,+reserve-x21` really
does keep the LFI-RISCV sandbox registers out of generated code, and that `+zba`
gives the `add.uw` the 4 GiB window is made of.

[`probe.sh`](probe.sh) reads that off the machine code rather than off the
documentation. Run it:

```
$ experiments/lfi-target/probe.sh
--- stock: instructions naming s2(x18)/s5(x21) ---
12
--- reserved: instructions naming s2(x18)/s5(x21) ---
0
--- zba in reserved (add.uw / sh1add) ---
4

ok: 12 stock, none reserved, 4 Zba instructions kept
```

The stock count has to be non-zero or the run fails: a probe the register
allocator never pushes far enough would report zero from both builds and look
like a pass. That is why [`probe.rs`](probe.rs) keeps twelve values live across
a call each — callee-saved registers are the only place they can go.

## The caveat, which the run prints itself

> warning: unknown and unstable feature specified for `-Ctarget-feature`:
> `reserve-x18` … it is still passed through to the codegen backend, but use of
> this feature might be unsound and the behavior of this feature can change

These are LLVM features passed through, not rustc features, and they can change
under us. The mitigation is that Molt does not have to trust the promise: the
loader re-derives it from the bytes, so a regression in the passthrough is a
rejected image rather than a silent hole.

## What it does not settle

The reservations have to reach `core`, which ships precompiled, and
`-C target-feature` on one crate does not rebuild it. That is the JSON target
plus `-Z build-std` in `docs/userspace.md`, and this experiment compiles a
single `#![no_std]` crate against the precompiled `core` instead — it measures
the compiler's behaviour, not the shape of the eventual build.

`rustc --print target-features --target x86_64-unknown-none` lists no
`reserve-*` feature at all, so `%r14` and `%r11` cannot be held back this way
and userspace goes to riscv64 first.
