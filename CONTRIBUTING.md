# Contributing

Molt uses the toolchain declared in `rust-toolchain.toml`.

Read [the style guide](docs/style.md) first; it is short, and it settles the
questions review would otherwise raise twice. [The testing
strategy](docs/testing.md) explains what each layer of the suite is for and
which layer a given change needs.

Before opening a pull request, run:

```console
just pre
just image
```

`just pre` checks formatting, lints the workspace, runs the host suite with
cargo-nextest, and clippy-checks both bare-metal kernel targets (`x86_64` and
`riscv64`). The kernel is deliberately excluded from the host workspace lint
because each platform crate defines a freestanding panic handler and the kernel
has no host entry point. When QEMU is installed, also run `just smoke`, which
boots the kernel on both architectures and asserts every serial marker through
`MOLT_BOOT_OK`; use `just smoke-x86_64` or `just smoke-riscv64` to boot a single
architecture (they need `qemu-system-x86_64` and `qemu-system-riscv64`
respectively). Any change to unsafe code must document its safety invariant and
add a test that exercises the safe API around that invariant. Performance
changes should include the benchmark command, machine details, and before/after
distributions rather than a single timing.

Changes to the lock-free primitives in `molt-core` additionally need `just
miri` and `just loom`. loom is minutes rather than seconds, so CI runs it on
main and on any pull request carrying the `loom` label — add the label when a
change touches an atomic ordering.

Changes to a parser that reads what a domain or a peer wrote need `just fuzz`,
which runs each libFuzzer target for a minute. It is not part of `just pre`
because a search that found nothing in a minute is not a pass — give it longer
(`just fuzz 600`) when the change is to the parsing itself rather than around
it. Property sweeps live next to the code they churn, share their runner
through `molt-churn`, and are expected to fail when the code is wrong:
`experiments/sweep-mutations.sh` puts a bug back under each one and checks that
it does. The same applies to `molt-arch/tests/contention.rs`, which runs the
machine-wide tables under eight threads: a change to what those tables do while
the lock is held belongs there rather than in a single-core test.
