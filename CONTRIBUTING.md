# Contributing

Molt uses the toolchain declared in `rust-toolchain.toml`.

Before opening a pull request, run:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench -p molt-core --no-run
cargo image
```

When QEMU is installed, also run `cargo smoke`. Any change to unsafe code must
document its safety invariant and add a test that exercises the safe API around
that invariant. Performance changes should include the benchmark command,
machine details, and before/after distributions rather than a single timing.
