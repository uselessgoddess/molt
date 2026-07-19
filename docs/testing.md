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
| Benchmarks | `just bench` | locally, on demand |
| Benchmark snapshot | `just bench-track` | main only |

Each layer catches a class the layer above cannot see. Unit tests catch logic.
Miri catches undefined behaviour on the paths a test happens to execute. loom
catches orderings the hardware happened not to produce. The smoke test catches
everything that only exists once there is a real machine underneath.

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
baseline. `just bench-track` emits libtest-format numbers and the `Benchmarks`
workflow preserves one 90-day artifact per main commit. The repository is
private and cannot publish GitHub Pages on its current plan, so a durable graph
is deferred until there is a store that can actually retain the series.

**Performance never gates the build.** Criterion's own FAQ advises against
gating CI on wall-clock numbers, and a shared GitHub runner is a virtualized
noisy neighbour: 10-20% between identical commits is normal. The snapshots are
there for manual comparison; the signal worth acting on is a change that
persists across several runs, not one spike. sel4bench takes the same position
— it keeps a JSON history and does not auto-fail on it.

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

## Boot tests

The smoke runner boots a real image under QEMU and asserts serial markers
through `MOLT_BOOT_OK`, with a hard 20-second timeout so a hang fails instead
of occupying a runner. Theseus and Redox both do a version of this — Theseus
boots under QEMU and checks an `isa-debug-exit` code, Redox hooks `redoxer`
into Cargo's target-runner so a kernel boot test is an ordinary `cargo` command.
The property all three share is worth keeping: the boot test is the same
artifact users get, not a special build.

One gap was worth closing. The panic handler is the single path a passing boot
never takes, so it could rot silently. `cargo smoke` now also boots a
`panic-smoke` build per architecture and requires both the `MOLT_PANIC:` marker
and a failure exit status.

## Conventions

Test naming and shape are in [the style guide](style.md). Two rules matter more
than the rest here:

- A concurrency test asserts a *property* — "the wake was not lost" — not a
  sequence of states. A test that pins down an interleaving passes for the
  wrong reason and blocks refactoring.
- Anything unsafe gets a test against the safe API around it, not against the
  unsafe function. That is what Miri and loom can then instrument.
