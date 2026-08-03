# Userspace and its toolchain

Status: Stage 5 decision record, August 2026.

[`docs/abi.md`](abi.md) decides what a user binary is. This document decides
what produces one, and what the first ones should be — the question in issue #70
was put as three options: a custom target, a fork of the Rust compiler, or
neither.

The answer is: **a custom target, no fork, and not `uutils` for a long while.**
The reasoning is below, and the part that decides it is a measurement rather
than an opinion.

## The shell already exists

It is worth starting here, because it changes the shape of the question.
`crates/molt-shell` is a `no_std` cell that acquires a directory capability,
submits `molt_fs::FsOp` on a ring, and awaits `FsDone`. Its own module
documentation states the rule it was written to:

> The shell is a client and nothing more. … the same interface any other cell
> would use, which is the point of having it: if `cat` needs something the
> protocol does not offer, the protocol is wrong.

So Molt does not need a shell. It needs its shell to run somewhere else. And
because that shell already speaks rings and capabilities and nothing else, the
move is a change of allocator and transport, not a rewrite. That is the strongest
evidence available that the ABI in [`docs/abi.md`](abi.md) is the right size:
its first user is a program that was written before it existed and needs nothing
added to it.

## Option 1: a custom target JSON

**Chosen.** Two target specs in-tree, built with `-Z build-std`.

The repository is already set up for it. `rust-toolchain.toml` pins
`nightly-2026-05-24` and installs `rust-src`, which is exactly what
`-Z build-std=core,alloc` needs; the workspace already builds two bare-metal
targets and `just pre` already checks both. A target spec adds a file, not a
prerequisite.

What the spec has to carry that flags cannot: the LFI register reservations must
apply to `core` too, and `core` ships precompiled. `-C target-feature` on the
crate does not rebuild it. A JSON target plus `-Z build-std` does.

That the reservations work at all was measured rather than assumed —
[`experiments/lfi-target`](../experiments/lfi-target):

```
--- stock: s2(x18)/s5(x21) uses ---
4
--- reserved: s2(x18)/s5(x21) uses ---
0
```

With `-C target-feature=+zba,+reserve-x18,+reserve-x21`, rustc stops using the
two registers the LFI-RISCV verifier reserves, and emits the Zba instructions
the scheme is built on. Stock compiler, pinned nightly, no patches.

The honest caveats, which the run prints itself:

> warning: unknown and unstable feature specified for `-Ctarget-feature`:
> `reserve-x18` … it is still passed through to the codegen backend, but use of
> this feature might be unsound and the behavior of this feature can change

These are LLVM features passed through rather than rustc features. They can
change. That is a real risk and the mitigation is the verifier: Molt does not
have to trust the compiler's promise, because the loader re-derives it from the
bytes. A regression in the passthrough becomes a rejected image, which is a loud
failure, not a silent hole.

## Option 2: a tier-3 target upstream

**Not now.** The
[target tier policy](https://doc.rust-lang.org/rustc/target-tier-policy.html)
opens with the requirement that decides it:

> A tier 3 target must have a designated developer or developers (the "target
> maintainers") on record to be CCed when issues arise regarding the target.

and adds that a target name, once chosen, is one the ecosystem uses. Molt's ABI
is version 0 and the sandbox does not exist yet. Putting `riscv64gc-unknown-molt`
in the compiler now would fix a name and a shape that the first three programs
are going to change.

What it would buy — `rustup target add`, and third-party crates building without
a JSON file — is worth having, and the moment to ask for it is when the ABI has
stopped moving and there is something to point at. It is the right *second*
step and the wrong first one. The policy also requires that a tier-3 target
"attempt to implement as much of the standard libraries as possible and
appropriate", which is the next section's problem and another reason to arrive
with an answer rather than a plan.

## Option 3: a rustc fork

**No — and the reason is architecture-specific, which is the surprise.**

A fork would be needed only if the register reservations cannot be expressed
with a stock compiler. On riscv64 they can, as measured above. On x86-64 they
cannot: `rustc --print target-features --target x86_64-unknown-none` lists no
`reserve-*` or `fixed-*` feature at all, and LFI-x64 needs `%r14` and `%r11`
held back. The only way to get that today is
[lfi-project/llvm-project](https://github.com/lfi-project/llvm-project) — "our
development fork of the LLVM project" — with a rustc built against it.

That is a multi-hour LLVM build in the path of every contributor who wants to
touch userspace, in a repository whose entire local check is a `just pre` that
runs on a laptop. It is not a technical impossibility; it is a cost the project
should not pay for a Stage 5 experiment.

So userspace goes to **riscv64 first**, and x86_64 keeps compiler-checked cells
until either upstream LLVM grows reserved-register features for x86 or consuming
the fork gets cheap. This inverts the usual ordering — x86_64 is Molt's mature
port, and [`docs/hardware.md`](hardware.md) argues it is where the real-hardware
work belongs — and the inversion is acceptable for a specific reason: the
sandbox does not touch drivers, and drivers are the entire reason x86_64 is
ahead. The two efforts do not queue behind each other.

One more thing falls out of the same asymmetry. `lfi-verifier`'s `src/` contains
`arm64`, `riscv64`, and `x64` directories, but its README documents only
`lfiv_verify_arm64` and `lfiv_verify_x64`. So on the architecture where stock
rustc suffices, the upstream verifier is the least settled — which turns
[`docs/abi.md`](abi.md)'s "write the verifier in Rust" from a preference into the
path of least resistance.

## What replaces `std`

Nothing, deliberately. A `molt-user` crate: `no_std`, `alloc`, a global allocator
over the sandbox's own heap, and typed wrappers over the `molt-abi` operations —
`Handle`, the ring client, and futures that complete when a `RequestId` comes
back. `molt-alloc` and `molt-exec` already exist and are already `no_std`, so
the sandbox runs the same allocator and the same executor as the kernel.

Porting `std` instead would mean writing a platform layer, and a platform layer
is a list of answers to questions Molt has deliberately not answered: what a
path is, what a process is, what a file descriptor is, what `fork` does. **This
is where POSIX would actually enter — through `std`, not through the ABI.** The
op table in [`docs/abi.md`](abi.md) can be kept short by refusing to add to it.
`std::process::Command` cannot be kept short; it either works or it does not,
and making it work means inventing processes for an OS whose thesis is that it
does not have them.

Keeping the user vocabulary equal to the kernel vocabulary has a second payoff:
a cell can move from inside the kernel to inside a sandbox, or back, as a
build-time decision. `molt-shell` is the test of that claim and it should be
kept true.

## coreutils

[uutils/coreutils](https://github.com/uutils/coreutils) is MIT-licensed, ~24k
stars, and by its own description "a cross-platform reimplementation of the GNU
coreutils" that "aims to be a drop-in replacement for the GNU utils. Differences
with GNU are treated as bugs."

That last sentence is the answer. Being a drop-in GNU replacement is not
incidental to uutils — it is the product. Its dependence on POSIX semantics is
not a porting detail to be worked around; it is the specification it is tested
against. Molt cannot take uutils without taking the interface uutils is a
replacement for.

So: **not now, and never in the kernel.** Two things follow.

If Molt ever wants uutils, the shape is fixed in advance: a `molt-posix`
*library inside the sandbox* implementing the slice of POSIX that uutils
touches, on top of `molt-abi` operations, plus a `std` platform layer that calls
it. Descriptors, paths, and `errno` live in the sandbox's address range and cost
the kernel nothing. The kernel never learns what a file descriptor is. Getting
this wrong in the other direction — POSIX operations in the op table because
`ls` wanted them — is the exact failure mode the issue asked to avoid, and it is
much easier to refuse now than later.

And there is a real argument for doing it *eventually*: uutils is a large,
genuine, third-party Rust workload, which is the best possible test of both the
ABI and the verifier. That is a strong reason to keep the door open and a bad
reason to walk through it first. The first program through the sandbox should be
one whose failures are about the sandbox.

## What ships instead, in order

| Step | Marker | What it is |
| --- | --- | --- |
| `riscv64gc-molt.json` + `-Z build-std` builds a `no_std` binary | `just user-check` | The target exists and `core` is rebuilt with the reservations |
| `molt-user` wraps the op table | host tests | A program can submit and await without touching `molt-abi` directly |
| `hello` runs in a sandbox and exits | `MOLT_SANDBOX_OK` | The loader from [`docs/abi.md`](abi.md) works end to end |
| `molt-shell` runs in a sandbox against the real filesystem ring | `MOLT_USER_SHELL_OK` | The claim above: a cell moved out of the kernel unchanged in shape |
| `cat`, `ls`, `cp` as Molt programs over `molt-abi` | `MOLT_USER_TOOLS_OK` | Enough of a userspace to notice what the protocol is missing |
| tier-3 target proposal | — | Only once the ABI version stops changing |
| `molt-posix` and uutils | — | Only if the compatibility exercise is judged worth it, and only in the sandbox |

## The decision, restated

- **Custom target**: yes, two JSON specs and `-Z build-std`, on the pinned
  nightly the repository already uses.
- **rustc fork**: no. riscv64 does not need one; x86_64 would, so x86_64 waits.
- **coreutils**: not uutils. Molt's own small tools first, over `molt-abi`; a
  POSIX layer, if ever, strictly inside the sandbox.
- **Shell**: the one already in the tree, moved rather than rewritten.
