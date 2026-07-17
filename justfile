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

x86_64-check:
    cargo clippy --package molt-kernel --target x86_64-unknown-none -- -D warnings

riscv64gc-check:
    cargo clippy --package molt-kernel --target riscv64gc-unknown-none-elf -- -D warnings

bench-check:
    cargo bench --package molt-core --no-run

pre: fmt-check lint test x86_64-check riscv64gc-check bench-check

image:
    cargo image

smoke:
    cargo smoke

bench:
    cargo bench --package molt-core
