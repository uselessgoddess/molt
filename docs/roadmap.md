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

- [x] RISC-V: map the kernel image per section instead of one RWX gigapage
- [x] RISC-V: use the SBI debug console (DBCN) with a legacy fallback

Both were Stage 1 shortcuts, and both were Stage 1.5 rather than Stage 2 work.

The gigapage was the one that mattered. `MapPermissions` rejects a
writable-and-executable mapping at construction, and the x86_64 platform
honoured that for the kernel image — but on RISC-V the running kernel executed
out of an identity-mapped RWX gigapage, so only the probe page was actually
W^X. A contract enforced on one platform and not the other is not a contract,
and Stage 2's DMA and drivers are exactly the code that turns a writable `.text`
into arbitrary execution. Retrofitting per-section permissions is also strictly
harder once drivers hold mappings. The boot mapping now walks the linker's
section bounds, and both platforms read their live tables back through
`Platform::verify_image_protection`, which prints `MOLT_WX_OK` — a marker the
smoke runner requires. `experiments/riscv-wx-regression` reintroduces the
defect to prove the audit can fail.

The console was smaller. The legacy `console_putchar` extension is deprecated in
SBI 0.2 and later, reports no errors, and costs one `ecall` per byte. It worked,
and it was isolated in `sbi.rs`, so it was not urgent — but Stage 2 debugging
leans on the console, and a console that cannot report its own failure is a bad
thing to be holding while chasing a driver bug. The port now probes the base
extension for DBCN, writes whole buffers through it, and demotes itself to the
legacy call if DBCN ever reports an error; `MOLT_SBI_CONSOLE:` names the winner
in the boot log, and `experiments/riscv-sbi-legacy-console` exercises the
fallback that QEMU's firmware never selects on its own.

Not in this stage: real-hardware boot. It needs serial-capture equipment that
does not exist yet, so QEMU stays the honest limit and the Stage 1 hardware
item stays unchecked rather than quietly reinterpreted.

## Stage 2 — First useful asynchronous I/O (`P1-stage-2`)

Stage 2 used to begin with PCI. It now begins with memory, because every item
below it asks a question Stage 1 could not answer: which frames does this queue
own, may this window be cached, and is the device still writing to the memory
being reused. Stage 1 represents physical memory as a `u64` handed out once to
the boot page table and never recorded again — enough for one consumer that
runs before interrupts, and not enough for a driver. `docs/memory.md` is the
decision record, including what was deliberately *not* taken from seL4,
Theseus, and Redox.

The sub-stages are ordered so that each one is the smallest thing the next one
cannot proceed without.

### Stage 2.0 — Typed physical memory

- [x] `Span`, `Kind`, and `Inventory`: physical memory typed from the firmware
      map, with device windows only where firmware left a hole
- [x] `Owner`, `Frames`, and `FrameTable`: one owner per frame, in
      caller-supplied storage, with no allocation in `molt-arch`
- [x] `Rights` and `Cache` split apart, W^X still rejected at construction
- [x] the live-table audit extended to device memory, failing closed on a leaf
      whose platform does not report its memory type
- [x] `MOLT_PHYSMAP_OK` and `MOLT_FRAME_OWNER_OK` on both platforms
- [x] `docs/memory.md`

### Stage 2.1 — A kernel-owned address space and the first MMIO window

- [x] x86_64 page tables owned by the kernel rather than the bootloader, so
      `Audit::accepts` runs on both platforms and not just RISC-V
- [x] cache attributes actually programmed into hardware: PAT on x86_64, and
      the `Svpbmt`/PMA question answered on RISC-V
- [x] a device window mapped through `Inventory::device`, with the UART as the
      first consumer that stops being an identity-mapped assumption

Nothing before this point maps a device. Nothing after it should map one
without the audit being able to see it.

### Stage 2.2 — PCI enumeration and interrupts

- [x] PCI configuration space enumerated through typed device windows
- [x] BARs sized non-destructively from the caller's point of view, and
      classified through `Inventory::device` before anything maps them
- [x] MSI/MSI-X vectors routed to the existing interrupt path, with the message
      minted by the platform fabric and unforgeable by a driver
- [x] `InterruptSlab`: arrivals counted in interrupt context, awaited as
      futures, with generations that refuse a stale token
- [x] `MOLT_PCI_OK` on both platforms; `MOLT_BAR_OK`, `MOLT_MSI_OK`, and
      `MOLT_INTERRUPT_OK` on x86_64, where an `edu` device proves an interrupt
      raised by a device actually reaches the slab
- [x] `docs/pci.md`

Two limits are recorded rather than checked off. Bus mastering is granted in
exactly one place — the kernel, for the one function whose MSI it routes —
because an MSI *is* a DMA write and a function that may not initiate
transactions cannot post one. Nothing in `molt-pci` sets the bit, but the
consequence is real: without an IOMMU that device is as privileged as the
kernel, and Stage 2.3 is where that trade has to be made explicitly. And RISC-V
mints no MSI vectors: its fabric reports `Unsupported` until there is an AIA
driver, so the RISC-V smoke enumerates and stops.

### Stage 2.3 — VirtIO block

- [x] a VirtIO block driver whose queues are `Owner::Device` frames
- [x] registered DMA buffers; no raw physical address in a public operation
- [x] cancellation, timeout, queue reset, and backpressure semantics
- [x] queue reset that reclaims frames only after the device is told to stop
- [x] `MOLT_VIRTIO_OK`, `MOLT_BLOCK_OK`, and `MOLT_VIRTIO_RESET_OK` on x86_64,
      where a signed virtio-blk disk proves a sector read completes through a
      ring the kernel owns and the reset returns its frames
- [x] `docs/virtio.md`

The read path is the whole path this stage builds. Stage 2.4's filesystem is
read-only, so the write side — a `VIRTIO_BLK_T_OUT` chain and the flush that
orders it — is deliberately absent rather than stubbed; see `docs/virtio.md`.

### Stage 2.4 — Something to run

- [x] `molt-block`: a `Device` trait every storage driver implements, so a
      filesystem never sees a virtqueue and a loopback disk tests it on the host
- [x] MoltROFS: a read-only, checksummed, extent-based format with a
      generation-stamped superblock kept in two copies
- [x] `FsOp`/`FsDone` over an `IoRing`, addressed by `Capability<Dir>` and
      `Capability<File>` with no paths and no ambient root
- [x] `cargo xtask mkfs <tree> <image>`, which lays a directory tree out as a
      mountable image
- [x] an async shell — `ls`, `cat`, `help` — driven by one task over that ring
- [x] `MOLT_FS_OK` and `MOLT_SHELL_OK` on x86_64, with the shell's own output
      required on the serial line so the markers cover disk to console
- [x] `docs/fs.md`

Acceptance: the kernel maps every device window through a typed, audited path,
completes block I/O through a ring using frames it owns, reclaims those frames
deterministically on reset, and prints a file from that disk through a
filesystem addressed only by capability — with the live-table audit passing on
both platforms.

Two decisions are recorded rather than checked off. The filesystem is not a cell
yet: the ring is the seam a cell boundary would need and it is load-bearing
today, but wrapping `Fs` in `Cell` before there is a second client and a remount
story would buy a layer whose content is the word "wraps". And the block driver
is called rather than awaited: a `BlockOp` ring worth having comes with
readahead and a cache, both of which want the writable filesystem's structure.
Both are argued in `docs/fs.md`.

## Stage 3 — Services and networking

- [x] a durable block write path — `molt-block::Write`, `Fault` power-loss
      injection, and the virtio `VIRTIO_BLK_T_OUT`/`VIRTIO_BLK_T_FLUSH` requests
      behind it — with crash-consistency tests that cut power at each checkpoint
      of the filesystem's two-copy superblock discipline
- [ ] a writable filesystem over it: object and data writes,
      `FsOp::Write`/`Create`/`Sync`, and the free-space map a copy-on-write
      checkpoint needs
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
