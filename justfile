set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: pre

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --exclude molt-kernel --all-targets -- -D warnings

test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Undefined behaviour, aliasing and provenance in the unsafe primitives.
miri:
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test --package molt-core

# Model-checks the lock-free primitives against the C11 memory model. Slow, so
# it runs on demand and on main rather than on every commit; the preemption
# bound keeps a full sweep to a few minutes.
loom:
    LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" \
        cargo test --package molt-core --profile loom --lib

x86_64-check:
    cargo clippy --package molt-kernel --target x86_64-unknown-none -- -D warnings

riscv64gc-check:
    cargo clippy --package molt-kernel --target riscv64gc-unknown-none-elf -- -D warnings

bench-check:
    cargo bench --package molt-core --no-run

pre: fmt-check lint test x86_64-check riscv64gc-check bench-check

image:
    cargo image

smoke-x86_64:
    cargo smoke x86_64

smoke-riscv64:
    cargo smoke riscv64

smoke: smoke-x86_64 smoke-riscv64

bench:
    cargo bench --package molt-core
