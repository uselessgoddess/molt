# Benchmarks

Status: Stage 4 decision record, July 2026.

What is measured, what a number is allowed to decide, and where the series
lives. [The testing strategy](testing.md) argued that benchmarks are worth
having and must not gate a build; this is what that turned into once there was
an executor worth optimising.

## Two metrics, and only one of them can fail a build

A shared GitHub runner is a virtualized noisy neighbour: 10-20% between
identical commits is ordinary, and no threshold distinguishes that from a real
regression on one run. So wall clock is kept as a series and never as a gate.

The other metric is deterministic. `crates/molt-exec/tests/cost.rs` installs a
counting `#[global_allocator]` and asserts what the hot paths ask of the heap:
a wake allocates nothing, an arm reuses the slot a cancelled deadline gave
back, an expiry allocates nothing, and every spawn costs the same as the last.
A counting `Machine` does the same for doorbells, where the cost is an
interrupt the host does not charge for. Those are ordinary tests. They fail in
`just test`, on any machine, with a number the runner cannot have an opinion
about.

The split is the point. Nanoseconds say where to look; the count is what a
change may not quietly spend. In a kernel the allocator is `molt-alloc` and the
path is one an interrupt is waiting at, so "how many allocations" is closer to
the property the design actually claims than "how many nanoseconds on a cloud
VM" ever gets.

The tally is a thread-local, not a static: the integration tests run in
parallel threads of one process, and a shared counter would measure all of
them.

## What is measured

| Bench | The path it stands for |
| --- | --- |
| `spsc_ring_round_trip` | one producer, one consumer, no atomics beyond the indices |
| `io_ring_round_trip` | a submission and its completion, which is every driver call |
| `cross_core_ping_pong` | the crossing: a ring between two cores |
| `completion_round_trip` | a slot claimed, completed, and reaped — compact and padded |
| `executor_wake_and_scan` | a wake and the scan that finds it |
| `executor_contended_wake` | the same under four threads, which is what padding is for |
| `atomic_waker_register_and_wake` | register, then wake from elsewhere |
| `spawn/local`, `spawn/remote` | a task made on its own core, and one handed across |
| `wake/one`, `wake/burst` | a waker rung once, and sixty-four rung at once |
| `timer/arm`, `timer/tick` | arming a deadline, and the tick that finds none due |
| `timer/expire`, `timer/cascade` | sixty-four deadlines coming due, and a level draining into the one below |

A bench earns its place by standing for something the kernel does per event,
not per boot. Everything here is a `no_std` library on the host, which is why
these numbers exist at all — see [the architecture note](architecture.md) on
what that does and does not let us claim.

Boot time is not on the list. It is a functional signal that QEMU mostly
decides, and treating it as a performance number would make a slower host look
like a slower kernel.

## The history

`cargo xtask bench` runs both packages' benches and writes one record:
the commit, the machine, and every bench's median, median absolute deviation,
mean, and confidence interval. The record carries a schema version, so a field
that changes meaning fails an old record loudly instead of drawing a wrong
line.

Records live on an orphan `benchmarks` branch under `data/`, one file per main
commit, named by date so that listing the directory sorts it by time.
`cargo xtask bench site data out` splices them into a static page — no
framework, no runtime dependency, no third-party action — which the workflow
publishes to Pages. A pull request gets `cargo xtask bench compare` as a step
summary table and nothing else.

![The latest run against the previous one and the first, then a series per
bench](bench-page.png)

The page above is that command over the five records the executor work was
measured against, which is also how it is read: what one commit did to every
bench first, and one bench across every commit after.

**Why not `benchmark-action`.** It stores a series and comments when a run
exceeds a ratio, which is a gate on wall clock measured where wall clock is
worth 10-20% of noise; the alert either fires on nothing or is set so loose it
never fires. Its page is also a chart per benchmark with no way to ask what one
commit did to all of them. [perf.rust-lang.org](https://perf.rust-lang.org) is
the model instead: a series per benchmark, a comparison between two chosen
commits, and the commit that moved it named on the same screen.

**A machine is part of the record.** Two runs from different CPUs are two
baselines, not a comparison, so the CPU model, architecture, toolchain, and
runner image are stored beside the numbers rather than assumed.

**A floor under the noise.** A change under 5% is reported as noise whatever
the spread claims. The spread is a median absolute deviation over criterion's
own samples, which measures the sampling and not the runner's neighbours.

## What a change has to answer

- Did a counted cost move? That is a test, and it is already red.
- Did a median move past the floor, and did it stay moved on the next run?
  One spike is a neighbour; two runs in a row is a change.
- Did anything else move with it? A win that pays for itself out of another
  bench is a trade, and the record shows both halves.

## The executor, measured

Stage 4.1 shipped an executor written for shape, not for cost. With the
records in place, five things were worth changing, each on its own commit with
its own numbers. AMD EPYC, one GitHub-class runner, medians:

| Change | Before | After |
| --- | --- | --- |
| timers in a slab, linked by index | `timer/arm` 75.0 ns | 48.4 ns |
| | `timer/tick` 35.4 ns | 25.1 ns |
| | `timer/expire` 534.7 ns | 435.0 ns |
| | `timer/cascade` 1.92 µs | 1.09 µs |
| room for a core's tasks taken up front | no wall-clock change | every spawn costs the same |
| the wake hands its reference on | `wake/one` 80.6 ns | 70.7 ns |
| | `wake/burst` 4.92 µs | 4.05 µs |
| a poll's waker stands on the poller's | `wake/one` 67.8 ns | 59.0 ns |
| | `wake/burst` 3.79 µs | 3.07 µs |
| one doorbell per burst | no wall-clock change | 64 wakes ring once |

The timers were the hypothesis with the most in it: an `Rc<Timer>` per deadline
and a `Vec<Rc<Timer>>` per bucket meant arming allocated, cancelling scanned,
and every one of them moved a reference count. They are now nodes in a slab
linked through it by index, so arming takes a slot off a free chain and
cancelling is a handful of writes.

**Generation counters, and why there are none.** A slot belongs to the one
`Sleep` that armed it until that drops, and dropping is also what frees it, so
no index can go stale while anything can still use it. The wheel is one core's,
so there is no second thread to lose a race to either. A generation counter
catches a reuse nobody here can observe.

The two wake changes are the same mistake twice: a reference count raised and
dropped for a reference the code already had. A wake arrives owning a strong
reference and used to hand back a clone of it; `schedule` now takes what it was
given. Every poll built a waker by cloning the task and dropped it on the way
out, whether the future kept the waker or not — the waker a poll is handed now
points at the reference the poller holds and is never dropped. A future that
keeps one still clones it, and the vtable is the same either way, so
`Waker::will_wake` still recognises the two as one waker.

**Batching completions.** A queue that drains sixty-four completions from one
interrupt wakes sixty-four tasks, and each of those used to ring the owner's
doorbell — sixty-four IPIs for work the owner does in one pass. The push now
reports whether it found the inbox empty, and only that push rings. The rest are
covered by it: a push onto a chain somebody else started either lands before the
drain, and is taken with the rest, or after it, in which case the swap already
emptied the head the exchange expected and the exchange failed. This is the one
change in the table with no wall-clock number, because on the host an IPI is a
function call that returns; what it costs is a cross-core interrupt, and what
proves it is `burst_rings_once` counting doorbells.

## Hypotheses the numbers did not support

Measured, and left alone. Each of these was worth doing if it were free; none
of them is.

**The inbox reversal.** Draining reverses the whole chain to turn a push stack
back into arrival order, which is O(n) and looked expensive. Dropping it
entirely — accepting LIFO, the best case any replacement could reach — moved
`wake/burst` by about 2%, roughly a nanosecond per task, because the reversal
is a second pass over nodes the first pass just pulled into L1. Arrival order
is worth more than that.

**Inline future storage.** A spawn allocates twice: the task, and the box its
future is pinned in. A `MaybeUninit<[u8; 256]>` in the task would remove the
second. Spawning a zero-sized future — which allocates only once, and so is
exactly the change's best case — is within noise of spawning a real one on this
host, because a same-size malloc/free pair off a warm tcache is a few
nanoseconds. The counted-cost argument survives the wall-clock one and is the
reason to revisit this: it would take spawn from two allocations to one, on an
allocator that is not glibc's. It is not a latency win, and 256 bytes per task
is 256 KiB on a core sized for a thousand of them.

**A waker with no reference count at all.** What is left after the two changes
above is one increment and one decrement per wake cycle, paid by futures that
re-register their waker on each poll. Removing it means tasks in a slab, wakers
as indices into it, and — unlike the timer slab — a waker really can outlive
its task, so this is where a generation counter would have to exist. That is a
new unsafe contract on the path every future takes, to save two uncontended
atomics on the same cache line. It waits for a bench that says they matter.

**Adaptive spin before parking.** A core parks the moment it finds nothing to
run, and spinning first would catch a wake that is about to arrive from a
neighbour. Whether that is a win depends entirely on how long the machine takes
to come out of `HLT` or `WFI`, and the host harness cannot answer: `Solo::park`
returns immediately, because there is no machine under it. This one is not
deferred for lack of interest — it is deferred for lack of a machine, like
everything else in the section below.

## Comparing against Linux

Molt has no bare metal, so a Linux comparison has to say which of its numbers
survive a hypervisor. Three tiers, of which two are available today.

**Same process, same CPU: the libraries.** `molt-core` and `molt-exec` are
`no_std` libraries that build on the host, so their paths can be measured
beside their Linux-side equivalents under one criterion harness, on one
machine, with one compiler — a wake against a current-thread runtime's wake, a
submission ring against `io_uring` through liburing. Nothing here is virtualized
and nothing is estimated. What it compares is the data structures, not the
kernels: no syscall, no interrupt, no scheduler crossing appears in it, and a
claim from this tier that does not say so is dishonest.

**Same VM, both sides: ratios only.** A device path can be measured under QEMU
if Linux is measured under the same QEMU — same command line, same virtio
model, same pinned vCPUs, same host — and only the ratio is reported. A KVM
guest takes an exit on a doorbell write that bare metal does not, so anything
that counts doorbells is partly measuring the hypervisor, and a difference
smaller than the emulation tax is not a difference. This tier is worth building
for Stage 4.4's `BlockOp`, where the question is how many requests are in
flight rather than how many nanoseconds one takes.

**Bare metal: the numbers the design is actually about.** Interrupt-to-wake and
tail latency cannot be taken in a VM at all, at any tier. The blocker is not
hardware ownership — an hour on a rented bare-metal instance is cheap — it is
that the run has to be reproducible unattended: boot an image, capture serial,
recover from a hang, and produce the same record format everything else here
produces. That is the "reproducible bare-metal benchmark runner" checkbox in
[Stage 4.7](roadmap.md), and it stays unchecked and unclaimed until it exists.

Meanwhile the counted costs hold everywhere, including on hardware nobody has
rented yet, which is the argument for having them.
