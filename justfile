set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: check

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo nextest run --workspace
    cargo test --workspace --doc

riscv-check:
    cargo clippy --package molt-riscv --target riscv64gc-unknown-none-elf -- -D warnings

bench-check:
    cargo bench --package molt-core --no-run

check: fmt-check lint test riscv-check bench-check

image:
    cargo image

smoke:
    cargo smoke

bench:
    cargo bench --package molt-core
