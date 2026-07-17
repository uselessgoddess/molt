set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: check

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --exclude molt-kernel --all-targets -- -D warnings

x86-check:
    cargo clippy --package molt-kernel --target x86_64-unknown-none -- -D warnings

test:
    cargo nextest run --workspace
    cargo test --workspace --doc

riscv-check:
    cargo clippy --package molt-kernel --target riscv64gc-unknown-none-elf -- -D warnings

bench-check:
    cargo bench --package molt-core --no-run

check: fmt-check lint test x86-check riscv-check bench-check

image:
    cargo image

smoke:
    cargo smoke

bench:
    cargo bench --package molt-core
