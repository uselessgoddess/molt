# Roadmap

The stages are ordered by dependency, not calendar date. A stage is complete
only when its acceptance checks run in CI and its unsafe invariants are
documented.

## Stage 0 — Bootable MVP

- [x] pinned Rust toolchain and reproducible Cargo workspace
- [x] host-tested `no_std` bounded SPSC and paired I/O rings
- [x] typed, restartable cell supervisor skeleton
- [x] x86_64 kernel with BIOS and UEFI images
- [x] serial boot marker and time-bounded QEMU smoke test
- [x] format, lint, unit, image, and boot CI
- [x] Criterion ring benchmark harness
- [x] architecture decisions and explicit security limits

## Cross-platform foundation

- [x] bootloader-independent memory-map and boot-information contract
- [x] hardware traits isolated from architecture implementations
- [x] x86_64 UART, halt, and test-exit implementation outside the kernel
- [x] RISC-V SBI console, panic, and shutdown implementation with a kernel target check
- [x] shared `just` command suite with nextest and bounded slow-test timeouts

## Stage 1 — Kernel foundations (`P0-stage-1`)

- [x] GDT/IDT and exception diagnostics with double-fault protection
- [x] physical frame allocator sourced from the boot memory map
- [x] owned virtual mappings with W^X policy
- [x] local APIC timer and monotonic tick source
- [x] interrupt-safe completion publication
- [x] minimal executor with a bounded ready queue and no lost-wakeup race
- [x] registered buffer capabilities; no raw DMA pointer in public operations
- [x] cell IDs, generations, typed capability rights, and revocation
- [x] per-cell arena ownership and deterministic restart sequence
- [x] QEMU tests for exception, timer, cancellation, stale completion, and restart
- [ ] documented real-hardware boot on one named x86_64 machine

Acceptance: the kernel boots without polling for device work, completes timer
futures through a ring, recovers a test cell without accepting stale results,
and passes all host/QEMU tests with no unreviewed unsafe block.

The remaining unchecked item requires a run on the named physical machine. Its
reproducible procedure and result fields live in the
[hardware validation record](hardware/amd-ryzen-9-9900x-gigabyte.md); no
successful hardware run is claimed before those fields are completed.

## Stage 1.5 — Hardening before Stage 2 (`P0-stage-1.5`)

Stage 2 adds drivers, DMA, and a filesystem on top of these primitives. Each
item here is cheaper to fix now than after something depends on it.

Testing and measurement:

- [x] loom model checks for the ring, completion slab, waker, and executor
- [x] cache padding as a measured, per-instance layout rather than an assumption
- [x] machine-readable benchmark snapshots retained per main commit
- [x] `molt-core` tested on aarch64, where atomics actually reorder
- [x] Miri on every push; loom on main and behind a label
- [x] the panic handler covered by a boot test, since a passing boot never
      takes that path
- [x] written style and testing conventions

Correctness debt:

- [ ] RISC-V: map the kernel image per section instead of one RWX gigapage
- [ ] RISC-V: use the SBI debug console (DBCN) with a legacy fallback

Both are Stage 1 shortcuts, and both are Stage 1.5 rather than Stage 2 work.

The gigapage is the one that matters. `MapPermissions` rejects a
writable-and-executable mapping at construction, and the x86_64 platform
honours that for the kernel image — but on RISC-V the running kernel executes
out of an identity-mapped RWX gigapage, so only the probe page is actually
W^X. A contract enforced on one platform and not the other is not a contract,
and Stage 2's DMA and drivers are exactly the code that turns a writable `.text`
into arbitrary execution. Retrofitting per-section permissions is also strictly
harder once drivers hold mappings.

The console is smaller. The legacy `console_putchar` extension is deprecated in
SBI 0.2 and later, reports no errors, and costs one `ecall` per byte. It works,
and it is isolated in `sbi.rs`, so it is not urgent — but Stage 2 debugging
leans on the console, and a console that cannot report its own failure is a bad
thing to be holding while chasing a driver bug.

Not in this stage: real-hardware boot. It needs serial-capture equipment that
does not exist yet, so QEMU stays the honest limit and the Stage 1 hardware
item stays unchecked rather than quietly reinterpreted.

## Stage 2 — First useful asynchronous I/O (`P1-stage-2`)

- [ ] PCI enumeration and MSI/MSI-X
- [ ] VirtIO block driver with registered DMA buffers
- [ ] cancellation, timeout, queue reset, and backpressure semantics
- [ ] read-only filesystem and an async shell cell
- [ ] deterministic integration tests using QEMU virtual devices

## Stage 3 — Services and networking

- [ ] writable filesystem and crash-consistency tests
- [ ] VirtIO network, Ethernet, ARP, IPv4, UDP, then TCP
- [ ] a typed scheme/resource namespace inspired by Redox
- [ ] capability delegation and audit events

## Stage 4 — SMP, hardware breadth, and performance

- [ ] per-CPU executors and rings; explicit cross-core fan-in
- [ ] allocator-backed executor stores and runtime capacity tuning
- [ ] IOMMU and device isolation where available
- [ ] NVMe and selected real NIC/storage targets
- [ ] reproducible bare-metal benchmark runner
- [ ] matched Linux io_uring throughput/tail-latency comparisons

## Stage 5 — Evolution experiments

- [ ] versioned C-compatible cell descriptor
- [ ] signed object loading with W^X mappings
- [ ] dependency namespaces and state migration
- [ ] atomic cutover, rollback, and fault-injection tests
