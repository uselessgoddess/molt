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

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --exclude molt-kernel --no-deps

miri:
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test \
     --package molt-core \
     --package molt-alloc

loom:
    LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" \
        cargo test --package molt-core --profile loom --lib

x86_64-check:
    cargo clippy --package molt-kernel --target x86_64-unknown-none -- -D warnings

riscv64gc-check:
    cargo clippy --package molt-kernel --target riscv64gc-unknown-none-elf -- -D warnings

bench-check:
    cargo bench --package molt-core --package molt-exec --package molt-block --package molt-fs --no-run

pre: fmt-check lint test doc x86_64-check riscv64gc-check bench-check loom

image:
    cargo image

smoke-x86_64:
    cargo smoke x86_64

smoke-riscv64:
    cargo smoke riscv64

smoke: smoke-x86_64 smoke-riscv64

bench:
    cargo xtask bench

# Compares the last run against a recorded one, as markdown.
bench-compare base:
    cargo xtask bench compare {{ base }}

# Builds the published page from a directory of records.
bench-site data out:
    cargo xtask bench site {{ data }} {{ out }}
