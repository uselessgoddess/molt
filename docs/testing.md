# Testing strategy

Status: Stage 1 decision record, July 2026.

What each layer of testing is for, why it exists, and what it deliberately does
not do. Written to answer three questions raised before Stage 2: is loom worth
it, are baseline benchmarks worth it now, and is multi-platform CI worth it
yet.

The short answers: yes, yes but not as a gate, and yes but tiered.

## The layers

| Layer | Command | Runs |
| --- | --- | --- |
| Unit and integration | `just test` | every push |
| Miri | `just miri` | every push |
| Concurrency model | `just loom` | main, or the `loom` label |
| Boot | `just smoke` | every push |
| Counted cost | `just test` | every push |
| Benchmarks | `just bench` | main, and on demand |

Each layer catches a class the layer above cannot see. Unit tests catch logic.
Miri catches undefined behaviour on the paths a test happens to execute. loom
catches orderings the hardware happened not to produce. The smoke test catches
everything that only exists once there is a real machine underneath. The
counted-cost tests catch a hot path that started asking the heap for something,
which is a benchmark a runner cannot argue with.

## Why loom

The primitives in `molt-core` are lock-free: a ring, a completion slab, an
`AtomicWaker`, and an executor whose slot states are compare-exchanged from
interrupt context. Their bugs are not logic bugs. They are one interleaving out
of thousands where a wake lands between a scan and a store, or where an
`Acquire` should have been an `AcqRel`.

A normal test cannot find those. It runs one interleaving, chosen by the
scheduler, on hardware that supplies orderings the code never asked for. Run it
ten thousand times and it explores a tiny, biased corner of the space and
reports green.

loom enumerates the space instead of sampling it. It replaces the atomics with
instrumented ones and runs the test body once per distinct execution the C11
memory model permits, so a missing `Release` fails deterministically on the
first run rather than in production on a machine nobody owns yet.

This is not speculative. The loom tests were validated by injecting the bug
they are meant to catch — weakening an ordering — and confirming the model
check fails.

**Cost, and how it is contained.** Exhaustive means exponential. The mitigation
is the standard one, taken from tokio: bound the preemption count.
`LOOM_MAX_PREEMPTIONS=2` keeps a full sweep to minutes while still covering the
interleavings that produce real bugs. That is still too slow for every push,
so it runs on main and behind a label rather than on the critical path.

**What a green loom run does not prove.** loom models C11, not any particular
CPU. It does not explore load-buffering executions, and it says nothing about
the code once it is compiled for a target and run under a real interrupt
controller. It raises confidence a long way; it does not replace running on
hardware that actually reorders.

**Shape of the integration.** `crates/molt-core/src/sync.rs` is a shim in the
style cordyceps and tokio use: the crate imports its atomics, `UnsafeCell` and
`spin_loop` from `sync`, which re-exports either `core::sync::atomic` or
`loom::sync::atomic` depending on `cfg(loom)`. Constructors use a direct
`cfg(loom)` branch: ordinary builds keep their `const fn`, while loom uses
`from_fn` because its atomics allocate model state.

## Why benchmarks, and why they are not a gate

The motivating question was concrete: `Executor` and `CompletionSlab` both hold
an array of contended atomics that is not cache-padded. Should it be?

Without numbers that is an argument. With them it is a trade: on a 4-core
x86_64 Linux VM, padding takes roughly 50% off `executor_contended_wake` and
adds roughly 8% to `completion_round_trip`, and costs 32 KiB of `static` memory
on `Executor<256>`. So layout is a per-instance type choice: `Executor<256>` is
compact and `Executor<256, Padded>` is cache-aligned. `CompletionSlab` exposes
the same choice. Both variants run in one benchmark binary, making the cost
visible without rebuilding the whole kernel with a different feature set.

That generalises. Benchmarks are worth having now, before Stage 2 adds drivers
and a filesystem, because the primitives they measure are the ones everything
later sits on, and because a baseline is only useful if it predates the change
you want to compare against.

**Keep a machine-readable history.** Criterion compares a run to one saved
baseline, which is one comparison and no series. `just bench` writes a record
per commit instead, and the `Benchmarks` workflow keeps them on a data branch
and publishes the graph.
**Performance never gates the build.** Criterion's own FAQ advises against
gating CI on wall-clock numbers, and a shared GitHub runner is a virtualized
noisy neighbour: 10-20% between identical commits is normal. The records are
there for comparison; the signal worth acting on is a change that persists
across several runs, not one spike. sel4bench takes the same position — it
keeps a JSON history and does not auto-fail on it. What does gate the build is
the counted cost, because an allocation is the same number on every machine.

## Why multi-platform CI

x86_64 is strongly ordered. It is the one architecture on which a missing
`Acquire` or `Release` cannot fail, because the hardware supplies the ordering
the code forgot to request. Testing lock-free primitives only there means the
suite is green on the machine least able to disprove it.

So the `atomics` job runs the `molt-core` suite on aarch64, which does reorder,
in both the padded and unpadded layouts. This is the cheapest available
hardware check on the orderings loom verifies in the model.

**Tiered, not gating.** Only the x86_64 `quality` job blocks a merge; the
aarch64 runners report without blocking. This is Redox's arrangement for its
non-primary targets, and the reason is practical: knowing aarch64 broke is
valuable, being unable to merge anything until it is fixed is not.

**No hardware CI, deliberately.** seL4 runs a 40-board hardware queue; its most
transferable idea is that the queue distinguishes an infrastructure-failure
marker from a test failure and retries only the former, because a hardware lab
that cannot tell "the board did not come up" from "the code is wrong" trains
everyone to ignore it. Molt has no boards and no serial capture equipment yet.
Until it does, QEMU is the honest limit, and the roadmap records the hardware
result as pending rather than claiming it.
[`docs/hardware.md`](hardware.md) prices the boards, designs the rig that would
capture their serial output with the markers this suite already defines, and
argues that the first such run should be a `just board` a person invokes — never
a merge gate — because a lab of one board has no queue to retry into.

## Boot tests

The smoke runner boots a real image under QEMU and asserts serial markers
through `MOLT_BOOT_OK`, with a hard 20-second timeout (`MOLT_SMOKE_TIMEOUT`
raises it for a slow host) so a hang fails instead of occupying a runner.
A timed-out run prints the serial log it captured, because the log is the only
evidence of where the boot stopped; the pipe is drained by its own thread so a
talkative guest cannot block on its own console and look like a hang. The smoke
path also does not pass `-no-shutdown`, which would turn a guest reset into a
silent hang rather than a reported exit status — see
`experiments/qemu-no-shutdown`. Theseus and Redox both do a version of this — Theseus
boots under QEMU and checks an `isa-debug-exit` code, Redox hooks `redoxer`
into Cargo's target-runner so a kernel boot test is an ordinary `cargo` command.
The property all three share is worth keeping: the boot test is the same
artifact users get, not a special build.

One gap was worth closing. The panic handler is the single path a passing boot
never takes, so it could rot silently. `cargo smoke` now also boots a
`panic-smoke` build per architecture and requires both the `MOLT_PANIC:` marker
and a failure exit status.

The x86_64 boot also attaches a modern VirtIO-net device to QEMU's user
network. `MOLT_NET_OK` requires the device to start with both queues routed to
MSI-X. The kernel then resolves its gateway by ARP and submits a DNS query
through the IP and UDP service rings. `MOLT_UDP_OK` is printed only after the
reply's endpoint, transaction ID, and response bit are checked. This is an
external packet round trip, not a loopback marker; wire parsing, capability
demultiplexing, restart invalidation, and RX reposting remain deterministic
host tests beneath it.

Two markers follow it, and both were chosen for what they cannot fake.
`MOLT_NDP_OK` requires a v6 datagram over slirp's `fec0::/64` leg whose `Sent`
completion cannot arrive until a solicitation is answered and the cache learns
the next hop, so the marker covers solicitation, advertisement, and cache in one
send rather than asserting that discovery code exists. `MOLT_TCP_OK` requires
the guest's own bytes back from `10.0.2.100:80`, which slirp forwards to `cat`
with no host listener involved, so a handshake, a segment out, a segment in, and
a close all have to work for it to print.

**Markers that only one machine can produce.** Stage 2.2 added the first ones.
`MOLT_PCI_OK` is required everywhere, but `MOLT_BAR_OK`, `MOLT_MSI_OK`, and
`MOLT_INTERRUPT_OK` are x86_64-only, because RISC-V mints no MSI vectors yet and
would have to fake one to print them. `arch_markers` is where that lives, beside
the RISC-V-only `MOLT_SBI_CONSOLE:`. The rule this follows is the same one the
hardware-boot item follows: a marker asserts a property the machine actually
has, and a machine that lacks it says so on the serial line rather than being
excused quietly.

**`MOLT_SATP_MODE: sv57` is an assertion about a value, not about a line.** The
riscv64 boot prints whichever paging mode the hart accepted, and the marker list
demands the widest one, so a probe that stopped early fails the smoke rather than
reporting a narrower address space in passing. It has a partner that is harder to
fake: `verify_owned_mapping` writes and reads its probe value at `1 << 54`, which
is 16 PiB and untranslatable under Sv39, so `MOLT_MAPPING_OK` on riscv64 is a
translation the hardware performed at an address only the wide mode reaches. See
[`address-space.md`](address-space.md).

**`MOLT_VA_OK` and `MOLT_ASID_OK` print numbers the hardware supplied.** Neither
is a fixed string: the first cuts the global VA allocator from the address width
the platform probed and carves the 100 GiB of
[`va-allocator.md`](va-allocator.md)'s worked example out of it, and the second
reports how many domain tags the hart actually implements. Both markers appear
on both platforms with different numbers — 57 bits and 65 535 tags on riscv64,
48 bits and none at all on x86_64's default model — which is the point: the
tagless path is not skipped, it is exercised, and the kernel that flushes on
every switch is proven to still work rather than assumed to.

**`MOLT_RAM_OK` catches a constant pretending to be a measurement.** The riscv64
kernel used to carry the QEMU `virt` default — RAM ends at `0x8800_0000` — which
boots identically on that one machine and is wrong on every other, in both
directions: more memory goes unused, less has the frame allocator hand out
addresses that decode to nothing. The smoke now starts QEMU with `-m 2G` and the
marker list demands `MOLT_RAM_OK: top 0x100000000`, a number the kernel can only
print by reading the `/memory` node of the device tree firmware passed. The
usable byte count follows the top rather than leading it, because that part moves
whenever the image in front of it changes size and is not something to pin.

**`MOLT_HUGE_MAP_OK` is read back, not remembered.** The size it prints does not
come from a variable the mapper set while building the tables — that would only
prove the mapper's intent — but from walking the live tables afterwards and
asking each address of every usable range which leaf translates it, which is the
same `PageWalk` the W^X audit uses. The walk stops on an unmapped page inside a
range the kernel declared rather than stepping over it, so a hole cannot hide
behind the bigger leaf next door. The asserted size differs per port because the
mappers do: riscv64 must show `1 GiB leaf`, which the smoke's `-m 2G` leaves
exactly one room for, and x86_64 must show `2 MiB leaf`, the largest its direct
map builds. Without this, a port that quietly fell back to 4 KiB pages would
still pass every other marker and cost only TLB misses, in a program nobody has
written yet.

**The x86_64 smoke boots `q35` with `-device edu`.** Both halves are load-bearing.
The default `pc` machine publishes no ACPI `MCFG` table, so there is no
configuration space to enumerate and the PCI smoke would pass by skipping
itself. And `edu` is the one function on the machine whose interrupt can be
raised on demand from software, which is what makes asserting a *delivery*
possible rather than just asserting that a capability was written. See
[`docs/pci.md`](pci.md).

**The block smoke runs behind `virtio-iommu-pci`.** The block function is
created with `iommu_platform=on`, stays unable to bus-master while its requester
is attached and its five DMA regions are mapped, and must negotiate
`ACCESS_PLATFORM`. `MOLT_IOMMU_OK` and `MOLT_IOMMU_MAP_OK` cover that ordering;
`MOLT_BLOCK_DEPTH_OK` requires two reads to be submitted before either is
reaped; and `MOLT_IOMMU_FAULT_OK` requires the replenished event queue to remain
clean through filesystem I/O and block reset. See [`block.md`](block.md).

**`MOLT_BLK_IRQ_OK` is a marker about an absence.** The block driver's used-ring
poll is gone, so a sector read that returns at all returns because queue zero's
MSI-X vector fired and the line counted it. The marker names the vector that
answered, and its value is that removing the interrupt path — not just breaking
it — now fails the smoke instead of falling back to a slower success.

**NVMe repeats the storage proof through a different transport.** QEMU receives
a second raw MoltFS image through its NVMe controller. `MOLT_NVME_IOMMU_OK`
requires all queue and PRP pages to be mapped before bus mastering;
`MOLT_NVME_DEPTH_OK` requires two commands live together; `MOLT_NVME_OK`
requires Identify plus read, write, flush, and readback; and
`MOLT_NVME_RESET_OK` requires controller disable before unmap and detach. Host
tests separately exercise command encoding, namespace formats, reordered
completion IDs, and failed status fields.

**The smoke disk is a filesystem, not a pattern.** Stage 2.4 replaced the signed
sector the virtio smoke used to read with a real MoltFS image: `cargo xtask
mkfs <tree> <image>` lays a host directory tree out as a mountable volume, and
the smoke builds one from the `disk/` tree in the repository. One artifact then
proves the whole path, because markers expose the same bytes at five heights:
`MOLT_BLOCK_OK` for the sector, `MOLT_FS_OK` for mount,
`MOLT_FS_WRITE_OK` for a durable create/write/sync/read cycle, then the shell's
own `molt> cat hello.txt` and `hello, molt` lines before `MOLT_SHELL_OK`.
Requiring data rather than only component markers says contents survived
driver, format, and ring. Stage 3 adds one more height above them:
`MOLT_FS_RESTART_OK` restarts the filesystem service on the same live device
and reopens the file the write cycle synced, so the marker says a crash-free
restart is as good as a checkpoint — and says it against a real disk, which no
host test can. See [`docs/fs.md`](fs.md).

**Two markers assert a recovery rather than a component.** `MOLT_REGISTRY_OK`
and `MOLT_WATCHDOG_OK` are printed by init between the shell's lines: the first
after the filesystem service restarts underneath a shell holding a lease on its
mount, the second after the shell misses two ticks and its own supervisor
restarts it unasked. Neither marker is the interesting assertion on its own —
what they buy is that `molt> cat hello.txt` and `hello, molt`, which the smoke
already required, are now printed by a cell that lost its service and then lost
itself and came back from both. A recovery that only prints its own success
marker proves that something ran; a recovery followed by required data proves
that what came back works.

Everything under those markers that can be tested without a machine is. The
`Device` trait has a `Loopback` implementation over bytes in memory, so
`molt-fs` mounts real images built by its own writer on the host, and `xtask`
lays out the smoke tree and mounts it back — which keeps the image honest even
where QEMU is not installed.

## Testing a budget nothing declares

Stage 3 added a layer the table above does not name: a test that measures stack
depth. The kernel gives the boot path 128 KiB and no guard page, so a call that
spends 78 KiB of frame is a fault waiting for a deeper caller — and nothing in
the type system says so. `crates/molt-fs/tests/stack.rs` paints a 96 KiB window
with `0xa5`, runs one filesystem call over the same frames, and reads back how
far down the mark was disturbed. Mount and commit each get 16 KiB.

Two things make it a test rather than a measurement. It fails on a regression
instead of printing a number nobody reads, and it names the limit that the
crash it prevents would not: a stack overflow in a kernel without a guard page
corrupts whatever lies below and reports something else entirely. It reads a
frame the compiler considers dead, which is exactly what Miri is right to
object to, so it is `cfg(not(miri))` and the safe API around it carries the
Miri coverage instead. See [the stack budget](fs.md#the-stack-budget).

The heap has the same shape of problem: `molt-fs` returns `FsError::Memory`
instead of panicking, and nothing exercises that path on a host with gigabytes
free. `crates/molt-fs/tests/memory.rs` is a separate binary because it installs
a `#[global_allocator]` that refuses allocations of a kilobyte or more — block
buffers and tree nodes, not the harness's own — and only on the thread that
asked for the refusal, so the tests still run in parallel. It shows a mount
answering the error and a create rolling back to its snapshot with the journal
still usable afterwards, which is the part a type signature cannot claim.

## Fuzzing, and the half worth having now

Stage 3 raised the question and answered half of it. `molt-net`'s parsers were
covered only by frames its own emitter wrote, so every length field they read
was one they had produced — the case that matters, a length field a peer chose,
was the one case never tested.

`crates/molt-net/tests/noise.rs` is that case, as an ordinary test rather than
as infrastructure. A xorshift generator seeded by one constant sweeps 16384
inputs per parser and asserts the invariant the parsers actually owe: nothing is
read past the input the caller handed over. The seed is the reproduction, so a
failure replays from a constant in the source with no checked-in corpus, no
crash triage, and no time budget in CI.

**The noise is shaped, because unshaped noise proves nothing.** Random bytes
almost never satisfy an IPv4 header checksum, and a random 16-bit length field
lands inside a 128-byte buffer about two times in a thousand — the first draft
passed with the truncation check deleted. So the sweep forces the version
nibble, zeroes the fragment field, repairs the checksum, and draws lengths from
a range that straddles the buffer's end, and every test asserts a floor on how
many inputs actually parsed. A sweep that proves nothing now says so.

Validated the same way the loom tests were: each of the three truncation checks
was reintroduced as a no-op in turn, and each time the sweep failed with an
index past the slice. Two further tests flip one bit of a valid packet and
require anything that still parses to re-emit to itself, which is the property a
parse/emit pair owes and neither half can be asked about alone.

**What is deliberately deferred.** A corpus, coverage-guided mutation, a CI time
budget, and crash triage are the other half, and they are infrastructure with
running costs. They earn those costs against a parser that takes input from
somewhere less bounded than a 1514-byte frame, which is Stage 4's NVMe and real
NIC work at the earliest. The cheap half runs on every push today.

## Red teaming the address space

Stage 5 pointed the same shape at what a hostile domain can reach. Each sweep is
an ordinary `#[test]` over a seeded xorshift, each asserts a floor on what it
covered, and each is named for the thing it is trying to break:

| Sweep | What it must not find |
| --- | --- |
| `molt-arch/tests/va_noise.rs` | an address handed out twice, or an arena that does not come back whole |
| `molt-arch/tests/refcount_noise.rs` | a count the model disagrees with, after any order of grant, revoke, split and merge |
| `molt-abi/tests/noise.rs` | a lying producer read past its fault, or a corrupt head starving the kernel end |
| `molt-arch/tests/shootdown_noise.rs` | a round nobody can close, or an address stuck in quarantine |

The refcount sweep carries a model that knows only which bytes are held how many
times — no classes, no records — so anything the table does with either, a split
that loses a count or a refusal that spends a slot, shows up as the two
disagreeing.

The last two are liveness claims, and a tracker that wedges does not crash: it
stops, and the addresses it holds are never handed out again. So they are made
the only honest way — from every state the churn reaches, drive the protocol
forward and see that it goes. `Shootdown` is `Copy`, so the escape runs on a
copy and the churn carries on from where it was.

## Conventions

Test naming and shape are in [the style guide](style.md). Two rules matter more
than the rest here:

- A concurrency test asserts a *property* — "the wake was not lost" — not a
  sequence of states. A test that pins down an interleaving passes for the
  wrong reason and blocks refactoring.
- Anything unsafe gets a test against the safe API around it, not against the
  unsafe function. That is what Miri and loom can then instrument.
