set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default: pre

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --exclude molt-kernel --all-targets -- \
        -W clippy::missing_safety_doc -W clippy::undocumented_unsafe_blocks -D warnings

test:
    cargo nextest run --workspace
    cargo test --workspace --doc

doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --exclude molt-kernel --no-deps

miri:
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test --package molt-core

loom:
    LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" \
        cargo test --package molt-core --profile loom --lib

x86_64-check:
    cargo clippy --package molt-kernel --target x86_64-unknown-none -- \
        -W clippy::missing_safety_doc -W clippy::undocumented_unsafe_blocks -D warnings

riscv64gc-check:
    cargo clippy --package molt-kernel --target riscv64gc-unknown-none-elf -- \
        -W clippy::missing_safety_doc -W clippy::undocumented_unsafe_blocks -D warnings

bench-check:
    cargo bench --package molt-core --no-run

pre: fmt-check lint test doc-check x86_64-check riscv64gc-check bench-check

image:
    cargo image

smoke-x86_64:
    cargo smoke x86_64

smoke-riscv64:
    cargo smoke riscv64

smoke: smoke-x86_64 smoke-riscv64

bench:
    cargo bench --package molt-core

bench-track:
    cargo bench --package molt-core -- --output-format bencher | tee bench-output.txt
