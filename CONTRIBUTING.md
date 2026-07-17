# Contributing

Molt uses the toolchain declared in `rust-toolchain.toml`.

Before opening a pull request, run:

```console
just check
just image
```

`just check` uses cargo-nextest for the host suite and separately checks both
bare-metal kernel targets. The kernel is deliberately excluded from the host
workspace lint because it defines a freestanding panic handler and has no host
entry point. When QEMU is installed, also run `just smoke`. Any change to unsafe
code must document its safety invariant and add a test that exercises the safe
API around that invariant. Performance changes should include the benchmark
command, machine details, and before/after distributions rather than a single
timing.
