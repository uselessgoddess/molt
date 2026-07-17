# Architecture direction

Status: MVP decision record, July 2026.

This document turns the constraints in issue #1 into testable design rules. It
is a direction, not a claim that the hard isolation, evolution, and driver work
is already solved.

## Goals and non-goals

Molt aims to be a small teaching OS that eventually boots real x86_64 hardware,
uses safe Rust ownership as its primary resource discipline, and represents
asynchronous work with bounded submission/completion rings. It should remain
easy to run under a host test runner and QEMU.


## What to take from existing systems

### Theseus

Theseus is the closest structural match. It runs components in a single address
space and privilege level, models cells as Rust crates, tracks dependencies, and
uses intralingual types to move OS invariants into the compiler. Its work on
state spill, namespaces, live evolution, and avoiding one component holding
another component's state is the main long-term reference.

Molt adopts small ownership-scoped cells and compiler-visible interfaces. The
MVP stops before runtime ELF loading: static linking gives one compiler and one
Rust ABI while the basic ownership model is tested.

### Redox

Redox is a Rust microkernel with processes, page-table isolation, userspace
drivers, and URL-like schemes. Its packet-based request model demonstrates the
value of one uniform interface for filesystems, devices, and services.

Molt adopts uniform typed operations and out-of-order correlated replies, but
not processes, syscall transitions, or copying IPC. Redox is also the useful
control case: it obtains fault containment from hardware boundaries that Molt
deliberately gives up.

### Linux io_uring

`io_uring` is built around two shared rings: the caller produces submissions and
consumes completions, while the provider does the reverse. Submission order and
completion order are separate, and caller-provided data correlates results.

Molt adopts that pair rather than treating a single queue as a request/response
channel. The MVP uses bounded SPSC lanes with acquire/release publication. It
does not claim Linux API or memory-layout compatibility.

### Hubris and capability microkernels

Hubris shows the value of a statically known task graph, bounded resources, and
supervised restart. It also makes the counterargument to Molt explicit: Hubris
uses hardware memory protection because memory safety alone cannot contain every
fault. Capability microkernels such as seL4 show how unforgeable handles can
make authority explicit even when the transport is generic.

Molt adopts static topology first, bounded allocation, generations on restarted
cells, and typed capability handles as the planned authorization mechanism. A
handle must name both an object and permitted operations; a registry lookup by
integer ID alone is not authorization.

## Ring design

`SpscRing<T, N>` owns initialized state and is split once into non-cloneable
producer and consumer endpoints. This is essential: a public `push(&self)` API
would allow multiple producers to write the same `UnsafeCell`, making the
pseudocode from issue #1 unsound.

The producer writes a slot and publishes `tail` with `Release`; the consumer
observes `tail` with `Acquire`, reads the value, and publishes `head` with
`Release`. Counter arithmetic wraps, while the live distance remains bounded by
`N`. Dropping the owner drops every still-initialized entry.

`IoRing<Op, Completion, N>` pairs two SPSC queues and exposes two views:

```text
client --Submission<Op>--> driver
client <--Completion<C>--- driver
```

Every entry is bounded and non-blocking. Backpressure is explicit through
`Result<(), Entry>`. `RequestId` correlates completions, which may arrive out of
submission order.

Planned rules:

- use one SPSC lane per producer/consumer pair; fan-in happens explicitly rather
  than silently turning the primitive into an MPMC queue;
- register buffers and pass typed buffer capabilities, not arbitrary raw
  pointers from general cells;
- allocate request IDs with a generation so stale completions cannot satisfy a
  reused slot;
- store wakers in a bounded slab, not a tree protected by a global mutex;
- in `Future::poll`, check completion, register/replace the waker, and check
  completion again to close the arrival-vs-registration race;
- specify cancellation, timeout, driver reset, and ring overflow behavior before
  adding real DMA.

“Everything async uses rings” does not mean every normal function call must be
queued. Pure calculations and lifecycle control remain direct typed calls.
Interrupt handlers and device work publish through rings; awaiting those results
is asynchronous.

## Cells, state, and recovery

The MVP `Cell` trait and `Supervisor` establish three properties: the supervisor
owns the cell, restart replaces owned state, and a generation changes on every
restart. Calls are statically typed and allocation-free.

The next stage adds arenas and capability revocation. A restart must:

1. stop new submissions to the old generation;
2. cancel or drain outstanding operations;
3. revoke exported capabilities;
4. drop the cell-owned arena as a unit;
5. spawn new state and publish a new generation.

A heartbeat detects liveness, not correctness. Restart cannot repair memory
corruption caused by unsafe code or DMA. Unsafe code should therefore be kept in
small audited hardware crates, with IOMMU protection considered when hardware
support begins.

## Component boundaries and ABI

Rust explicitly gives the native Rust ABI no stability guarantee, and default
Rust representation does not guarantee field order. A trait object also embeds
compiler-private vtable layout. Therefore `Arc<dyn CellApi>` is acceptable only
inside one statically linked image built by one compiler; it is not a live-update
ABI.

The first dynamic boundary should be deliberately small:

- a versioned `#[repr(C)]` descriptor with `abi_version` and `struct_size`;
- `extern "C"` function pointers that never unwind;
- integer capability handles and byte spans described by pointer/length pairs;
- fixed-width `#[repr(C)]` request/result records with explicit status codes;
- allocator ownership callbacks when memory crosses the boundary;
- no Rust references, slices, enums without explicit representation, generics,
  `Future`, or trait objects across versions.

Static cells remain the default. Dynamic loading is justified only after symbol
resolution, W^X mappings, signature policy, dependency versioning, rollback,
and state migration have tests.

## Security model

Safe Rust can prevent broad classes of memory bugs and encode ownership, but the
compiler is not a sandbox. The trusted computing base includes the compiler,
bootloader, all unsafe code, inline assembly, interrupt setup, allocators,
drivers, and devices capable of DMA. A panic in a critical cell can still deny
service; a logic bug can still misuse legitimate authority.

The honest first-stage security statement is therefore: one mutually trusting
image, compiler-enforced memory safety where code is safe, explicit capability
authorization as it is added, and no containment guarantee against compromised
trusted code.

## Performance and comparison

Criterion benchmarks cover host-side queue cost and provide distributions. They
are regression signals, not Linux comparisons. A defensible Linux comparison
must use the same machine, device, queue depth, operation sizes, CPU affinity,
warm-up, compiler settings, and durability semantics. It must report throughput,
median and tail latency, CPU time, cache misses, and variance.

QEMU boot time is a functional CI signal only. Real-hardware benchmarks begin
after timer calibration, interrupt-driven I/O, and a production-capable driver.

## Primary references

- [Theseus design and cell structure](https://www.theseus-os.com/Theseus/book/design/design.html)
- [Theseus OSDI 2020 paper](https://www.usenix.org/conference/osdi20/presentation/boos)
- [Redox source and architecture entry points](https://github.com/redox-os/redox)
- [Efficient IO with io_uring, Jens Axboe](https://kernel.dk/io_uring.pdf)
- [Hubris reference and robustness philosophy](https://hubris.oxide.computer/reference/)
- [seL4 capability overview](https://sel4.systems/About/)
- [Rust Reference: type layout](https://doc.rust-lang.org/reference/type-layout.html)
- [Rust Reference: external ABIs](https://doc.rust-lang.org/reference/items/external-blocks.html#abi)
- [Rust x86_64 bare-metal target](https://doc.rust-lang.org/rustc/platform-support/x86_64-unknown-none.html)
- [rust-osdev bootloader API](https://docs.rs/bootloader/0.11.15/bootloader/)
