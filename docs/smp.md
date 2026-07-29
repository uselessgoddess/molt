# SMP and the executor

Status: Stage 4.0–4.3 decision record, July 2026.

Why a core shares nothing with its neighbours, what the four questions an
executor asks the machine are, and what "shared nothing" costs where it is not
free. Written as the record for `molt-core::cpu`, `molt-core::peers`, the
`molt-exec` crate, `molt-alloc::Global`, `molt-arch`'s `Local`/`Smp` traits and
`kernel/src/smp.rs`.

## The decision

**Cores share nothing.** A core owns its executor, its tasks, its timer wheel
and its heap. Work does not migrate. A core reaching another one is a message
on a ring that pair alone holds, plus a doorbell.

The alternative is the work-stealing pool tokio and Go run, and it is the
better answer when the workload is many short tasks of wildly unequal cost. It
charges `Send + 'static` on every future to get there — here that means on
`Cell`, on the B-tree's `Rc`, on every service holding a borrowed device — and
it buys load balancing Molt cannot yet spend, because the work is bounded by
devices and there is one of each.

What that buys back is that almost nothing in the kernel is concurrent. The
ready queues are `VecDeque`, the task table is a `Vec`, the wheel is
`Rc<Timer>` in `RefCell`. None of them is locked, because there is nobody to
lock them against. The atomics are confined to what a neighbour can touch:
three of them and a stack, in `exec::Shared`.

Stealing is worth revisiting when there is a workload with more runnable work
than devices. That is a measurement, not a design.

## Four questions

`molt-exec` names the machine under it as a trait, and it is deliberately tiny:

```rust
fn cpu(&self) -> CpuId;
fn wake(&self, cpu: CpuId);
fn park(&self);
fn ticks(&self) -> u64;
```

Which core is this, ring that one, stop until someone rings, what time is it.
Every one is a register read or an interrupt. The executor asks for no memory,
no mapping and no lock, which is what lets the same code run on a host under
`Solo` — one core, no doorbell, a clock the test advances — and be tested there
without a machine.

`Machine` is `Sync` and taken as `&'static dyn`. One static answers every core
because none of the four questions is answered out of a field: identity comes
from `gs` on x86_64 and `tp` on RISC-V, the doorbell is the target's own block,
and the tick is the count that core's timer interrupt bumped.

## Identity costs a load

`CpuId` is a dense index counted from the boot core, not the number the machine
uses. APIC IDs are sparse and hart IDs start wherever firmware likes; a
platform maps whatever it was handed onto this at start, and nothing above ever
learns which one it was.

Per-CPU blocks are one static array, not an allocation: the platform crates run
before the heap exists. `gs`'s base is the address of the core's element and
the element's first word points back at itself, so `gs:[0]` is one load rather
than an `rdmsr`. RISC-V keeps the same layout under `tp`.

That answer has to exist before anything else per-core does, which is why it is
the first sub-stage. `kernel/src/heap.rs` routes an allocation by
`smp::here()`, and `here()` is answerable on a core that has attached nothing.

## Bringing a core up

An application processor comes out of reset knowing nothing — no tables, no
long mode, not even a stack. On x86_64 the boot core copies a trampoline blob
into a frame under a megabyte and sends INIT-SIPI-SIPI; the blob's only
absolute address is its own, taken from `cs`, so every label stays the offset
the assembler already computed. Two things it must not get wrong, both of which
cost a triple fault: the descriptor table register is loaded with the table's
own linear address rather than the frame's, and no-execute goes on with long
mode, because the kernel marks every data page with a bit that is reserved to a
core which never enabled it.

RISC-V has none of that: `hart_start` takes an entry and one opaque word, so
the shim there exists only to carry a stack and a hand-off the SBI call has no
second register for.

A started core lands in `smp::enter`, which is three things and nothing else:
its own tick, its own executor, and a handle left where the others can find it.
The starter waits on that handle with its doorbell armed and a deadline to give
up on — a core that never reports costs the parallelism it would have brought
and nothing else.

## What crosses

`Handle` is the whole of what a neighbour can reach: an `Arc<Shared>` and the
owner's `CpuId`. `Handle::spawn` is where `Send + 'static` is asked for, and
the only place — a task that never leaves its core is never made to prove it
could.

A task is an `Arc` whose future is touched by one core only. The owner's task
table holds a reference until the future is dropped, so a waker released on
another core frees memory and never a future. That is what lets a task that is
not `Send` be woken by a core it could not run on.

The inbox is a Treiber stack, one per priority level: a push is a
compare-exchange and a drain is one swap per turn, rather than a lock per wake.

Fan-in with an answer wants the opposite shape. `Peers<T, N, P>` gives one SPSC
ring per peer, because a shared inbox is a shared cache line and a shared cache
line is every core in the machine taking turns at one address whether or not
they have anything to say to each other. A ring per sender costs `P` times the
memory and none of that, and a full ring refuses that one sender rather than
the service behind all of them. The owner's own slot is there too, so a core
handing work to itself takes the same path as one that does not.

## Priority and the wheel

Three levels — high, normal, low — and they are queues, not deadlines: a high
task runs before a normal one that is also ready and never instead of it. Each
level gets a slice of polls (32, 8, 2) before the next one's turn, because a
busy device queue must not be able to starve the rest.

Deadlines live in a hierarchical wheel: four levels of sixty-four slots, so
arming is a shift and a push and a tick looks at one slot instead of every
deadline. A deadline further out than a level can express waits in the level
above and cascades down when the lower wheel wraps, so a timer moves a bounded
number of times no matter how far ahead it was set. Ticks are the caller's and
the wheel walks one slot per tick, which means the unit should be the
scheduling quantum rather than a cycle counter.

Both were built with the cores rather than after them, on purpose. A priority
added later is a second ready queue and a second inbox on a path two cores
already share; a wheel added later is a deadline representation every parked
core has to agree on. Neither is a change that stays inside one file.

## Parking

The last spin in the kernel is gone. A core with nothing ready calls `park`,
and the implementation must arm before it halts: the doorbell flag is read with
interrupts off and the halt only happens if it was clear, so a wake that raced
the decision is taken rather than slept through.

A wake rings the flag and sends the interrupt only when the flag was not
already set — an unanswered wake is one the target has yet to look at, and a
second one tells it nothing the first did not. Waking oneself rings and stops
there, because the interrupt would only arrive where the caller already is.

## Interrupts with an affinity

A vector is allocated for a core, not for the machine: the fabric encodes the
target's APIC into the MSI address, so the interrupt lands where the service
that waits on it runs. Submission, interrupt and completion on one core is what
makes a ring a local data structure again — the cache line stays put and the
wake is a poll rather than an IPI.

The device line is bound before the device is programmed, never after: a device
able to deliver into a line nobody owns is a dropped interrupt at best.
Arrivals are counted rather than signalled, so an interrupt that beats the
waiter to the slab is not a lost wakeup.

`Registry` publications name the core that answers them, because reaching a
service is reaching the core that owns it, and a client that learned the name
without the core would have to ask somebody else where to send.

## The heap

One heap per core, picked by a `Router` the kernel implements as `smp::here()`,
so two cores allocating at once wait on nothing.

Freeing is the interesting half. A block carries the index of the heap it came
out of in the low bits of its back pointer, so a release names its owner. A
release on the owning core takes that heap; a release anywhere else is pushed
onto the owner's stack — a single store, drained by the owner on its next
allocation. That is also exactly what an interrupt handler needs, which is why
the interrupt path is the same mechanism rather than a second one: a free
inside a handler is a push whether or not the interrupted core was holding its
own heap's lock.

Allocating inside one is not. `interrupt` bars this core's heap for as long as
the guard is held and a request made under it is refused — a null rather than a
wait on a lock the core it interrupted will not release until the handler
returns. Only that one heap is barred; the other cores never notice. A core
whose own heap is spent does borrow from the next one, which is a different
case: bytes sitting unused a shard over are not a reason to fail.

## What this leaves undone

- **Work stealing.** Not a gap: a decision, revisitable against a measurement.
- **A cell that moves between cores.** The bound is on the handoff and the
  handoff exists; nothing calls it yet, because no supervisor has a reason to.
- **MSI-X.** The affinity path is the fabric's, and it routes MSI and MSI-X the
  same way. The device the smoke has implements MSI, so that is the one the
  test drives.
- **More than eight cores.** `MAX` is a constant over a static array and
  nothing else; the number is a place to grow, not a design.

## What proves it

The smoke starts every core firmware described, waits for each to report an
executor, then sends a task to each one and asserts the answer carries the
identity that core's own block reports — proof the task ran there, rather than
proof a counter moved. The device vector is homed on a peer and the wait is
spawned there, and both halves are checked: the core that answered is the one
the line was homed on, and the core that took the interrupt is the same again.

```
MOLT_SMP_OK: cores=4 answered=3
MOLT_AFFINITY_OK: line 0 on core 1
```

Four cores rather than two on purpose: a crossing that only ever runs on two is
a crossing that never had to pick a target.
