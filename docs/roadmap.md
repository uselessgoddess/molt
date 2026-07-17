# Roadmap

The stages are ordered by dependency, not calendar date. A stage is complete
only when its acceptance checks run in CI and its unsafe invariants are
documented.

## Stage 0 — Bootable MVP (this pull request)

- [x] pinned Rust toolchain and reproducible Cargo workspace
- [x] host-tested `no_std` bounded SPSC and paired I/O rings
- [x] typed, restartable cell supervisor skeleton
- [x] x86_64 kernel with BIOS and UEFI images
- [x] serial boot marker and time-bounded QEMU smoke test
- [x] format, lint, unit, image, and boot CI
- [x] Criterion ring benchmark harness
- [x] architecture decisions and explicit security limits

## Stage 1 — Kernel foundations (`P0-stage-1`)

- [ ] GDT/IDT and exception diagnostics with double-fault protection
- [ ] physical frame allocator sourced from the boot memory map
- [ ] owned virtual mappings with W^X policy
- [ ] local APIC timer and monotonic tick source
- [ ] interrupt-safe completion publication
- [ ] minimal executor with a bounded ready queue and no lost-wakeup race
- [ ] registered buffer capabilities; no raw DMA pointer in public operations
- [ ] cell IDs, generations, typed capability rights, and revocation
- [ ] per-cell arena ownership and deterministic restart sequence
- [ ] QEMU tests for exception, timer, cancellation, stale completion, and restart
- [ ] documented real-hardware boot on one named x86_64 machine

Acceptance: the kernel boots without polling for device work, completes timer
futures through a ring, recovers a test cell without accepting stale results,
and passes all host/QEMU tests with no unreviewed unsafe block.

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
- [ ] IOMMU and device isolation where available
- [ ] NVMe and selected real NIC/storage targets
- [ ] reproducible bare-metal benchmark runner
- [ ] matched Linux io_uring throughput/tail-latency comparisons

## Stage 5 — Evolution experiments

- [ ] versioned C-compatible cell descriptor
- [ ] signed object loading with W^X mappings
- [ ] dependency namespaces and state migration
- [ ] atomic cutover, rollback, and fault-injection tests
