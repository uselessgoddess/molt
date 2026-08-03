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
      ([`docs/hardware.md`](hardware.md) costs it out and argues this is the
      cheapest real-hardware result available, and cheaper than any RISC-V
      board)

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

This stage originally built only the read path needed by Stage 2.4. Stage 3
extends the same queue with `VIRTIO_BLK_T_OUT` and durable flush; see
`docs/virtio.md`.

### Stage 2.4 — Something to run

- [x] `molt-block`: a `Device` trait every storage driver implements, so a
      filesystem never sees a virtqueue and a loopback disk tests it on the host
- [x] the read-only MoltFS v1–4 predecessor, retired by the unified writable v5
      checkpoint format in Stage 3
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

One decision is recorded rather than checked off. The block driver is called
rather than awaited: a `BlockOp` ring worth having comes with readahead and a
cache, both of which want the writable filesystem's structure. The other — the
filesystem is not a cell yet — held only until there was a remount story, which
Stage 3 supplies. Both are argued in `docs/fs.md`.

## Stage 3 — Services and networking

- [x] writable filesystem and crash-consistency tests
- [x] `molt-alloc`: a kernel heap, and the B-tree moved onto it
- [x] DMA regions a driver returns and reuses, not held until reset
- [x] the filesystem started, served, and restarted as one service
- [x] a typed scheme/resource namespace inspired by Redox
- [x] capability delegation and audit events
- [x] interrupt-driven VirtIO network, Ethernet, ARP, IPv4, and capability-addressed UDP
- [x] IPv6, ICMPv6 echo, and neighbor discovery through the same cache and routes
- [x] TCP behind the same link and service boundary — see [`docs/net.md`](net.md)
- [x] block completions awaited on MSI-X instead of a spin budget

Writable filesystem includes sector writes, required virtio flush support,
three rotating checkpoint-log banks, a checksummed copy-on-write metadata
B-tree with bounded node caching and generation reclamation, deterministic
tree/log/flush/root-swing/flush ordering, one typed tree representation for
mkfs and runtime mutations, live-payload compaction, `Create`/`Write`/`Sync`
capability operations, and fault injection that cuts power before every
checkpoint and reclamation action. Mount always selects a complete old or new
generation and never depends on fsck. `MOLT_FS_WRITE_OK` proves the same path
through QEMU's virtio-blk device.

Networking follows the ring-first boundary rather than adding sockets to the
kernel. `molt-net` owns Ethernet, ARP, IPv4, IPv6, ICMPv6, neighbor discovery,
and protocol capabilities; `molt-udp` owns port demultiplexing and socket
capabilities; `molt-tcp` puts smoltcp behind the same `Link`; and the kernel
only maps modern VirtIO-net and routes its MSI-X entry through
`InterruptSlab`. `MOLT_UDP_OK` requires a checked DNS reply to cross the real
device and both service rings. `MOLT_NDP_OK` requires a v6 datagram that cannot
leave until an advertisement resolves its next hop, so the marker proves
solicitation, advertisement, and cache in one send. `MOLT_TCP_OK` requires a
handshake, an echo, and a close through slirp's forwarder.

IPv6 is one address family, not a second stack: the link dispatches on
EtherType, `neighbor::Cache` is the one table ARP and NDP both fill, and `UdpOp`
carries an `Endpoint` holding either address. Nothing above `addr` knows which
it holds, which is what made the family cheap enough to add now rather than
after Stage 4 had multiplied every path by a core count.

TCP is where the stack stops being written here. Congestion control,
retransmission, and reassembly are algorithms with decades of corrections in
them, so [smoltcp](https://github.com/smoltcp-rs/smoltcp) supplies them behind a
`phy::Device` that is a thin shim over `Link`. What Molt keeps is the part
smoltcp has no opinion about: `TcpOp`/`TcpDone` on a ring, streams named by
`Capability<Socket>`, and a `TcpCell` whose restart drops every stream a dead
client held. The dependency ends at the segment; the boundary does not move.

The block driver's used-ring poll is gone. Queue zero takes an MSI-X vector of
its own, and each command waits on `Arrivals` — the trait whose kernel
implementation is the same `InterruptSlab` future the network path uses — then
drains every used entry the arrival covered, since one interrupt is not one
completion. The spin that remains is in the kernel's stand-in for a scheduler,
not in the driver: `wait` polls the real future because there is nothing to park
a task on yet, which is Stage 4.1's job and no longer the driver's shape.
`MOLT_BLK_IRQ_OK` names the vector that answered.

The three items after it are what that filesystem then demanded. A B-tree whose
nodes and paths lived in fixed arrays spent 78 KiB of a 128 KiB stack with no
guard page beneath it, so the kernel grew a heap — first fit, address-ordered,
coalescing — donated 4 MiB of frames at boot, and the tree moved onto it;
a test now holds mount and commit under 16 KiB each. A driver that could only
return DMA frames by resetting its queue grew per-region release. And `FsCell`
makes the filesystem a service with a lifecycle rather than a library every
caller links: it mounts once, answers on a ring, and restarts at the last
durable checkpoint with every handle from the old epoch revoked. Being the first
real cell, it is also what `Cell` was measured against and rewritten for —
fallible `spawn`, restart in place through the supervisor's hooks, the message
pair moved out to `Handler`, and no thread bounds on a service that owns a
borrowed device. See [`docs/memory.md`](memory.md) and [`docs/fs.md`](fs.md).

The namespace is the item that turned init into something other than a script.
`molt_core::registry` is a `Registry<S, N>` of typed endpoints: a service
publishes one under a `Scheme` — `Storage` is the only one today — and a client
acquires a `Capability<S>` lease that names the publication rather than the
endpoint, so a service that restarts leaves every outstanding lease `Stale`
instead of pointing at a mount that is gone. No string is parsed anywhere in it,
which is the whole argument in [`docs/fs.md`](fs.md) about what *typed* was
meant to buy. With a place to look things up, the shell stopped being wiring in
the kernel and became a cell: init publishes, the shell acquires, the service
restarts underneath it, and the shell meets `CapabilityError::Stale`,
re-acquires, and carries on — `MOLT_REGISTRY_OK` on the serial line is that
round trip. Two cells then made a policy possible that one could not:
`Supervisor::watch` compares a tick against the heartbeat a cell last reported
and restarts what has gone quiet, so the smoke test contains one restart nobody
asked for on the line above it, reported as `MOLT_WATCHDOG_OK`.

Delegation and audit events are the other half, and now done. `CapabilityTable::delegate`
attenuates a capability and hands it to a second cell in one step: `To` must be a
subset of both the source type and the slot's live rights, so no copy outgrows
its source, and revoking the owner stales every copy at once. Possession is the
authority to delegate — the delegator is audited, not checked against the slot's
owner, so a delegate can delegate further, exactly as a capability system wants.
`audit::Log` is the record of who did: a bounded ring of `Grant`/`Delegate`/`Revoke`
events that overwrites its oldest entry under pressure and counts what it dropped,
so a full log is visibly lossy rather than silently short. Delegation is the one
authority change a capability's value does not already reveal — a grant returns
its handle, a revoke its count — so it is the one the log exists to catch.

Asynchronous I/O below the ring stays off this list, and now for a reason
stronger than review size. `Volume` and `Journal` still call the block device
and block. A `BlockOp` ring that only ever holds one request buys nothing over
the call it replaces; what makes it worth its ordering rules is readahead and
parallel extent fetch, and both are several submissions in flight at once,
which is a scheduler. So it moves to Stage 4.4, behind the executors — see
`docs/fs.md`.

Fuzzing arrived as tests rather than as infrastructure. `molt-net`'s parsers
were covered only by frames its own emitter wrote, so every length field they
read was one they had produced; `crates/molt-net/tests/noise.rs` now shapes
noise past the version and checksum checks and asserts nothing is read past the
input, with the seed as the reproduction. That is the half that pays for itself
today. A corpus, a CI time budget, and crash triage are the other half, and
they are background infrastructure for whenever there is a parser worth that —
see [`docs/testing.md`](testing.md).

## Stage 4 — SMP, hardware breadth, and performance

One decision shapes the rest: **cores share nothing**. A core owns its
executor, the cells on it, and the rings between them; work does not migrate,
and a wake that crosses a core is a message plus an IPI, not a stolen task.

The alternative is the work-stealing pool tokio and Go run, and it is the
better answer when the workload is many short tasks of wildly unequal cost. It
charges `Send + 'static` on every future to get there — which here means on
`Cell`, on the B-tree's `Rc`, on every service holding a borrowed device — and
it buys load balancing Molt cannot yet spend, because the work is bounded by
devices and there is one of each. Shared-nothing is also the cheap answer here
rather than the austere one: a ring is already how two things that share no
state talk, and a second core is only a further-away peer. Seastar and glommio
take this position for throughput; seL4 and Theseus take it because the
alternative crosses an isolation boundary. Stealing is worth revisiting when
there is a workload with more runnable work than devices — that is a
measurement, not a design.

The sub-stages are ordered so that each one is the smallest thing the next one
cannot proceed without.

### Stage 4.0 — A core that can name itself

- [x] per-CPU blocks reached through `gs` on x86_64 and `tp` on RISC-V
- [x] application processors started — INIT-SIPI-SIPI, SBI HSM `hart_start` —
      onto the page table the boot core already owns
- [x] a per-core tick, and an IPI minted by the platform the way an MSI is

Nothing below is per-core until "which core am I" has an answer that costs a
register read, and nothing parks until a core can be woken by another one.

### Stage 4.1 — One executor per core

- [x] an `Executor` per core, allocator-backed, sized at runtime rather than by
      a const generic chosen at compile time
- [x] halt on an empty ready queue, woken by the IPI instead of the spin that
      stands in for a scheduler today
- [x] `Send` and `'static` on the handoff that moves a cell between cores, and
      nowhere else
- [x] three priority levels with a slice each, and a hierarchical timer wheel

The bounds go on the mover, not on `Cell`: a cell that never leaves its core
should not have to prove it could. This is also the stage that deletes the last
`wait(token, spins)` — the interrupt future is already the right shape, and
what is missing is only somewhere to park.

Priority and the wheel were built here rather than deferred, and that is the
one place this stage spends more than the minimum. Added later, a priority is a
second ready queue and a second inbox on a path two cores already share, and a
wheel is a deadline representation every parked core has to agree on; neither
change stays inside one file.

### Stage 4.2 — Rings and interrupts with an affinity

- [x] MSI-X vectors routed to the core that owns the service behind them
- [x] cross-core fan-in as an explicit ring per peer pair, with no shared queue
- [x] `Registry` publications naming which core answers

Submission, interrupt, and completion on one core is what makes the ring a
local data structure again — the cache line stays put and the wake is a poll,
not an IPI. A vector landing on the wrong core is a correctness non-event and a
performance disaster, which is why it is a checkbox.

The affinity is the fabric's and it routes MSI and MSI-X the same way; the
device the smoke has implements MSI, so that is the one `MOLT_AFFINITY_OK`
drives.

### Stage 4.3 — An allocator that is not one lock

- [x] per-core free lists over the address-ordered first-fit that exists, with
      remote frees queued to the owning core rather than taken under its lock
- [x] `MOLT_SMP_OK` and `MOLT_AFFINITY_OK` on both platforms, where four cores
      answer a crossing with the identity their own blocks report
- [x] `docs/smp.md`

Sharding happens under the lock, not through the filesystem's types: `Rc` and
`&mut` inside the B-tree stay, because a service reached only by ring is
already the unit a core owns.

Work stealing is recorded rather than checked off, and so is a cell that
actually moves: the bound is on the handoff and the handoff exists, but nothing
calls it until a supervisor has a reason to. Both are argued in `docs/smp.md`.

### Stage 4.4 — Asynchronous `BlockOp`

- [x] `Volume` and `Journal` awaiting a `BlockOp` ring instead of calling the
      device and blocking
- [x] readahead and parallel extent fetch as concurrent submissions

The first workload that needs more than one request in flight, which is why it
waits for a scheduler that can hold them.

The buffer travels with the request, so the queue owns no borrow and a
completion hands the block back to whoever asked. Above it a volume keeps eight
of them: a sequential read asks for the blocks after the one it waits on, and a
region walk — a mount verifying a checkpoint, a commit summing what it wrote —
spends every free slot on the blocks ahead of the one it is handing over.


### Stage 4.5 — Device isolation

- [x] typed `Iova`, `DeviceId`, permissions, and consuming mappings; no raw
      address can enter a virtqueue descriptor
- [x] identity and fake mapper backends, with overlap, reuse, device-scope,
      permission, and double-unmap tests
- [x] VirtIO-IOMMU attach/map/unmap/detach plus a permanently replenished fault
      event queue
- [x] `VIRTIO_F_ACCESS_PLATFORM` required for translated VirtIO block DMA
- [x] eight independent virtio-blk requests, out-of-order completion matching,
      device error propagation, interrupt completion, timeout/stale ownership,
      and flush barriers through the stable `BlockOp` contract
- [x] QEMU `q35` smoke with `virtio-iommu-pci`, `iommu_platform=on`, bus
      mastering enabled only after mappings exist, two reads live together,
      clean fault reporting, and ordered teardown
- [x] [`docs/block.md`](block.md)

Acceptance: a block endpoint is attached and mapped before it can bus-master;
every descriptor carries an IOVA derived from a live permissioned mapping;
multiple operations are demonstrably in flight; and device reset precedes
unmap, detach, and frame reuse. MoltFS's format and durability ordering are
unchanged.

### Stage 4.6 — Hardware breadth

- [x] NVMe Identify, admin/I/O queue pairs, eight live block operations, and
      read/write/flush behind `molt_block::Queue`
- [x] VirtIO block, VirtIO network, and NVMe requester IDs isolated in distinct
      bounded IOMMU domains
- [x] QEMU NVMe smoke with mappings before bus mastering and reset before unmap
- [x] one module owns IOMMU bring-up and teardown for every endpoint
      (`kernel/src/isolation.rs`), so the ordering that is the isolation
      guarantee exists in one place rather than once per driver
- [ ] selected real NIC/storage targets ([`docs/hardware.md`](hardware.md):
      no board in the affordable RISC-V class enumerates PCI the way this
      kernel does, and none has an IOMMU, so the selection waits on a
      non-ECAM host-bridge path)

NVMe reuses the existing `molt_block::Queue` and `Mapper` boundaries. Namespace
discovery and PRP/queue-pair setup remain driver work; no filesystem operation
or raw DMA escape was added. Both storage drivers expose depth eight so the
current comparison is like-for-like queue occupancy. Real-hardware throughput
and tail latency remain Stage 4.7 measurements.

### Stage 4.7 — Numbers

- [x] a record per commit, published, with a counted cost the tests can gate on
- [ ] reproducible bare-metal benchmark runner
- [ ] matched Linux io_uring throughput/tail-latency comparisons

Last, deliberately. A number measured before the shape settles is a number
about the wrong program, and the io_uring comparison is only honest once
submission, completion, and interrupt sit where they will stay. The series
came first anyway, because the executor could not be made faster without it.

## Stage 5 — Evolution experiments

- [ ] versioned C-compatible cell descriptor
- [ ] signed object loading with W^X mappings
- [ ] dependency namespaces and state migration
- [ ] atomic cutover, rollback, and fault-injection tests

### Stage 5.1 — The user binary ABI

- [x] [`docs/abi.md`](abi.md): LFI as the isolation mechanism, a `molt-abi`
      crate for the versioned `repr(C)` descriptors, rings instead of syscalls,
      and channels that keep the kernel off the IPC data path
- [x] [`docs/userspace.md`](userspace.md): a custom target JSON with
      `-Z build-std`, no compiler fork, and no `uutils` in the kernel
- [x] [`experiments/lfi-target`](../experiments/lfi-target): stock rustc holds
      back the registers LFI-RISCV reserves
- [ ] `molt-abi` with asserted layouts, and `molt-user` over it
- [ ] a verifier in Rust, agreeing with the reference on its own corpus
- [ ] a sandbox that loads, runs, and exits (`MOLT_SANDBOX_OK`)
- [ ] a rejected image that never becomes executable
      (`MOLT_SANDBOX_REJECT_OK`)
- [ ] `molt-shell` running in a sandbox against the real filesystem ring
      (`MOLT_USER_SHELL_OK`)

riscv64 leads this stage, which is the reverse of every stage before it: a
stock rustc can reserve the registers LFI-RISCV needs and cannot reserve the
ones LFI-x64 needs. The limit worth writing down is not the 4 GiB per sandbox
the scheme imposes but the 44 GiB of address space each one reserves, which
Sv39 has room for roughly five of; Sv48 in the RISC-V paging module is the fix,
and it is Molt's own code.
