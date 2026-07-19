# Molt

Molt is a learning operating system exploring two constraints:

- one compiler-checked address space with no process abstraction;
- paired submission/completion rings as the asynchronous I/O primitive.

The current kernel boots on x86_64 through BIOS or UEFI, installs protected
exception tables, verifies an owned W^X mapping, and completes a local-APIC
timer future through a typed ring. It then exercises cancellation, stale-result
rejection, and cell restart before printing `MOLT_BOOT_OK` on COM1. The
architecture-independent ring, executor, capability, and cell lifecycle code
is a `no_std` library so it can be tested and benchmarked on the host. Hardware
interfaces are defined separately from x86_64 and RISC-V implementations, so
kernel orchestration contains no port I/O or bootloader-specific types.

> Molt is research software, not a security boundary. Safe Rust reduces memory
> safety risk, but a single address space does not contain unsafe code, DMA,
> logic errors, or malicious components. See [the architecture notes](docs/architecture.md).

## Repository layout

```text
crates/molt-arch/          boot and hardware contracts; no platform implementation
crates/molt-core/          no_std rings, executor, capabilities, and supervised cells
crates/platforms/x86_64/   x86_64 boot, exceptions, paging, local APIC, UART, and exit
crates/platforms/riscv/    RISC-V SBI console and shutdown implementation
kernel/                    architecture-independent kernel orchestration
xtask/                     reproducible image builder and QEMU smoke runner
docs/                      architecture, roadmap, testing strategy, and style
```

## Prerequisites

- [rustup](https://rustup.rs/) (the pinned toolchain is installed automatically)
- [just](https://just.systems/) and
  [cargo-nextest](https://nexte.st/) for the development command suite
- `qemu-system-x86_64` for `boot` and `smoke`

The dated nightly in `rust-toolchain.toml` is intentional: `bootloader` builds
custom BIOS stages and depends on nightly compiler details. Host tests and the
kernel source do not otherwise rely on unstable language features.

## Build and test

```console
just pre
just image
```

Images are written to `target/molt/molt-bios.img` and
`target/molt/molt-uefi.img`.

Run the automated BIOS boot assertion:

```console
just smoke
```

Or show the serial/QEMU monitor interactively:

```console
cargo boot
```

Run the ring microbenchmarks with:

```console
just bench
```

To try real hardware, follow the
[Stage 1 hardware validation record](docs/hardware/amd-ryzen-9-9900x-gigabyte.md).
Double-check the target device: imaging a disk overwrites its partition table.
The automated path is continuously smoke-tested in QEMU; the named-machine
result remains explicitly recorded as pending until it is run by its owner.

## Design status

Stage 1 intentionally keeps cells statically linked and uses typed Rust calls
inside one compiler build. It does **not** pass Rust trait-object vtables across
versions. Future dynamic cells need a small, versioned `repr(C)` descriptor and
capability handles, as described in
[the ABI section](docs/architecture.md#component-boundaries-and-abi).

See [the roadmap](docs/roadmap.md) for first-stage acceptance criteria and later
milestones, [the testing strategy](docs/testing.md) for what each layer of the
suite is for, and [the style guide](docs/style.md) for the conventions rustfmt
and clippy cannot check.
