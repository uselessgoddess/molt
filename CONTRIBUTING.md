# Contributing

Molt uses the toolchain declared in `rust-toolchain.toml`.

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
