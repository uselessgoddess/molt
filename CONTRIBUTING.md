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
