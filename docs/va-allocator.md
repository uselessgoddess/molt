# The global VA allocator

[`docs/address-space.md`](address-space.md) chose a single address space with
three tiers, and listed "a global VA allocator" as one of three things that has
to change for it to work. This document is that subsystem's design, kept
separate because it is the piece the whole tiering rests on: if virtual
addresses cannot be handed out, recycled, and kept globally unique at a sane
cost, the SASOS argument collapses and Molt is back to one page table per
program.

The code is [`crates/molt-arch/src/va.rs`](../crates/molt-arch/src/va.rs); the
claims below are pinned by [`crates/molt-arch/tests/va.rs`](../crates/molt-arch/tests/va.rs).

The host tests are not the whole evidence. `MOLT_VA_OK` runs the same round trip
inside a booted kernel, over a `Space` cut from the width the hardware admitted
to rather than a constant: carve the 100 GiB the tier-2 example asks for, prove
it came back aligned and in 100 leaves, release it, prove the addresses are
*not* reissued before a sweep, retire the epoch, and prove the exact same
address comes back afterwards. On riscv64 that prints `57 address bits, 100 GiB
at 0xe0000000000000 in 100 leaves`; on x86_64, 48 bits and
`0x700000000000`. Both are in the marker list `xtask` requires, so the
allocator is a shipped subsystem rather than a tested library.

## The questions, answered first

| Question | Answer | Section |
| --- | --- | --- |
| Buddy allocator? | **No** — its class structure duplicates the alignment classes and rounds a 100 GiB extent to 128 GiB | [candidates](#the-candidates) |
| Slab / size classes? | **No** — extents are not same-size objects and a size class does not imply a leaf alignment | [candidates](#the-candidates) |
| Compaction? | **Never.** A global address is the product; relocating it would break the one property being sold | [compaction](#compaction-the-answer-is-no-and-why-that-is-safe) |
| Fragmentation after churn? | Bounded, because grant/revoke does not allocate at all, and what does allocate coalesces on release | [fragmentation](#fragmentation-under-grantrevoke-churn) |
| What if it runs out? | A reported error with a number attached, not a stall — and the numbers say when | [arithmetic](#the-arithmetic-how-much-there-is) |
| Where does freed space go? | Quarantine, until every hart has flushed the epoch it was freed in | [quarantine](#a-freed-address-is-not-a-free-address) |

## What makes this problem different from `malloc`

Four properties, none of which a heap allocator has to deal with, and all four
push the design in the same direction.

**There is exactly one of it.** A per-process VA allocator can be greedy, can
leak, and can be reset by process exit. This one is machine-wide and lives as
long as the boot does, so anything it leaks is leaked forever and any fragment
it creates is permanent. Process exit is not a garbage collector here.

**Freeing is not free.** A heap `free` makes bytes reusable immediately. A
virtual address that was just unmapped may still be cached in some hart's TLB;
handing it to a different domain before every hart has flushed is a
cross-domain read, which is exactly the failure the design exists to prevent. So
release and reuse are separated by a shootdown, and the allocator has to model
that.

**Alignment is not a hint, it is the whole point.** A 100 GiB mapping is worth
having because it is 100 page-table entries instead of 26 214 400, and it is 100
entries only if it starts on a gigabyte boundary. An allocator that hands out a
correctly sized but misaligned range has silently turned a gigabyte mapping back
into a page mapping.

**The addresses are the interface.** Two domains granted the same extent see it
at the same address, so a pointer stored *inside* mapped data stays valid across
the grant. That is the porting requirement from
[`docs/address-space.md`](address-space.md), and it means an address, once
handed out, cannot be moved.

## What the allocator is actually asked to do

1. Hand out ranges aligned to the leaf size they will be mapped with (4 KiB,
   2 MiB, 1 GiB), without searching for the alignment.
2. Hand out a hundred-gigabyte range as *one* extent, not as a list.
3. Never hand out an address that some hart may still have a translation for.
4. Run before there is a heap, because the address space is needed to build one.
5. Coalesce, so that a long-lived system does not slowly become unable to
   satisfy a large request while showing plenty of free space.
6. Refuse loudly. Running out of address space must be an error a caller can
   see, never a silently smaller or misaligned answer.
7. Stay off the hot path. If domain-to-domain communication had to allocate,
   the "faster than Linux" claim would be about the allocator instead.

## The candidates

| Policy | Verdict |
| --- | --- |
| Buddy | **No.** Rounds to a power of two and re-invents alignment classes |
| Slab / size classes | **No.** Extents are not same-size objects |
| Bitmap of granules | **No.** 2^40 bits for the page class alone |
| Balanced tree of ranges (Linux VMAs) | **No.** Needs a node allocator and answers a question Molt does not ask |
| Per-hart magazines over a global pool | **Later.** An optimisation of the below, not an alternative |
| **Alignment-class arenas, address-ordered first fit, immediate coalescing** | **Chosen** |

**Buddy.** Splitting a power-of-two range in halves gives O(log n) coalescing
with no search and alignment equal to size for free, which sounds like exactly
what a leaf-aligned allocator wants. Two things kill it. The alignment it gives
is *too much*: a 100 GiB extent needs 1 GiB alignment, and buddy would round it
to a 128 GiB block, wasting 28 GiB of address space per mapping and — worse —
making the waste grow with the request. And the structure it imposes is a second
class system layered on the one the page-table levels already impose; the three
arenas below *are* a buddy tree pruned to the three levels that mean something
to the hardware, with the useless levels removed rather than carried.

**Slab and size classes.** Slabs pay off when many objects of one size are
allocated and freed and construction is expensive. An address range has no
construction cost, and extent sizes are dictated by what is being mapped — a
ring is 64 KiB, a domain heap is a few megabytes, a mapped file is whatever the
file is. Size classes would round requests up (the buddy problem again, in a
weaker form) and would still not give the leaf alignment, because a size class
is not an alignment class: 3 MiB rounded to a 4 MiB class says nothing about
where the range starts.

**A bitmap.** One bit per granule is simple and constant-time-ish to scan. The
page-class arena at Sv57 is 4 PiB, which is 2^40 pages, which is 128 GiB of
bitmap. Even the gigabyte class costs 1 MiB of bitmap to describe 8 PiB. A free
list costs bytes proportional to the number of *holes*, which is the quantity
that stays small.

**A tree of ranges.** Linux keeps VMAs in a maple tree (formerly a red-black
tree) because it must answer "which mapping contains this faulting address"
millions of times a second. Molt maps eagerly — the whole argument for extents
is that there is no fault storm — so the address→mapping lookup is not on any
hot path, and a tree buys ordering Molt gets from a sorted array anyway. A tree
also needs nodes, and requirement 4 says the allocator runs before the heap
exists.

**Per-hart magazines.** Give each hart a private chunk of each arena and it
allocates without touching the global lock. This is a real optimisation and it
is deliberately not in the first version: it costs address space per hart per
class and it only pays if allocation is frequent, which requirement 7 says it
must not be. It is written down here so that if profiles ever say otherwise, the
answer is known and the interface does not change.

## The decision

**One arena per leaf class, address-ordered first fit inside each, coalescing on
release, over caller-supplied storage.**

```
Space::over(bits, &mut holes)

  [ 3·2^(bits-3) ................................. 2^(bits-1) )
  |   Page arena    |   Mega arena   |        Giga arena       |
  |   4 KiB leaves  |  2 MiB leaves  |       1 GiB leaves      |
  |     quarter     |     quarter    |           half          |
```

**Why classes rather than one arena.** Alignment stops being a search. Every
arena bound and every carve is a multiple of that arena's granule, so alignment
is an invariant of the free list rather than a property some search has to
re-establish. `allocate(Class::Giga, 100 GiB)` cannot return a misaligned
address, because there is no misaligned address in that arena to return.

**Why the classes cannot borrow from one another.** Letting the gigabyte arena
spill into the megabyte arena would immediately reintroduce the search — the
spilled region would have to be gigabyte-aligned inside a range that is not — and
would let one runaway consumer starve a class it does not use.
`an_exhausted_class_does_not_borrow_from_another` pins this: filling the
gigabyte arena leaves the megabyte arena whole, and the failure is
`Error::Exhausted` rather than a quiet fallback.

**Why the gigabyte class gets half.** The classes are sized by ratio rather than
by absolute numbers, because the hart's reported width decides how much there is
at all. The class whose extents are the largest gets the most room; the page
class hands out kilobytes and does not need petabytes to do it.

**Why address-ordered first fit.** It is what `molt-alloc`'s heap already does,
so this is one policy in the kernel rather than two, and the empirical
literature is unusually clear about it: Johnstone and Wilson's survey of real
program traces found address-ordered first fit and best fit within a couple of
percent of perfect, and most published fragmentation disasters to be artifacts
of synthetic random workloads. First fit also has the property that matters
after a long run: it keeps allocations packed at low addresses, which leaves the
large contiguous ranges at the top intact for the requests that need them.

**Why the storage comes from the caller.** `Space::over` takes a `&mut [Hole]`
exactly the way `FrameTable::over` takes its slots. No allocator, no
bootstrapping cycle, and a fixed, auditable memory cost.

## A freed address is not a free address

Release does not return addresses to circulation. It stamps them with the
currently open shootdown `Epoch` and parks them:

```
release(extent)  ->  hole { start, end, ready: open }      // unusable
sweep()          ->  closes the batch, returns its epoch   // "flush this"
   ... every hart executes its sfence.vma / INVLPG ...
retire(epoch)    ->  everything ready <= epoch is usable again
```

Three consequences worth stating plainly:

**Batching is the point.** A shootdown is an IPI round to every hart; paying one
per unmapped extent would make revocation the dominant cost of a domain
teardown. `sweep` closes a batch, so a domain that releases four hundred extents
pays one flush for all of them. `an_epoch_that_was_never_swept_frees_nothing`
pins the ordering: `retire` on an epoch that is still open is a no-op, so a
mistaken call cannot shortcut the flush.

**Coalescing is restricted to equal epochs, and this is not a detail.** The
first implementation merged a freed range into any adjacent hole and took the
later of the two epochs. That is correct and catastrophic: freeing one gigabyte
next to a free arena tail moved the *entire tail* behind the next flush, so a
single release could drop the allocatable space of a class to zero. The rule is
now that only neighbours waiting on the same epoch merge on release, and
`settle` — run from `retire` — merges the islands once they have become
indistinguishable. `released_addresses_wait_for_the_shootdown` and
`churn_leaves_no_permanent_fragmentation` are the two halves of that fix.

**Quarantine is bounded by the release rate, not by the arena size.** At most
what has been released since the last retire is unusable, and
`Space::quarantined(class)` reports it, so a kernel that quarantines too much is
a number on a marker rather than a mystery.

## Fragmentation under grant/revoke churn

The worry is a system running for weeks on 32 harts, handing memory between
domains, whose address space slowly turns to lace. Three separate arguments say
it does not, in order of how much work each does.

**Grant and revoke do not allocate.** This is the load-bearing one. A grant
installs an *existing* extent's translations into a second view; a revoke
removes them. The address was allocated once, by whoever created the mapping,
and it does not change hands — that is what "globally unique address" means. So
the churn rate the allocator sees is the rate at which *mappings are created and
destroyed*, not the rate at which they are shared. A domain that grants the same
ring buffer to a thousand peers a second allocates exactly once.

**What does allocate is coarse and long-lived.** A domain's heap, a mapped file,
a ring: units of megabytes to terabytes, created at setup and destroyed at
teardown. There is no per-message, per-page, or per-request allocation on any
path in [`docs/abi.md`](abi.md) — the ring ABI moves data through memory that
was mapped before the first message.

**And what churn there is coalesces.** `churn_leaves_no_permanent_fragmentation`
runs eight rounds of six extents of six different sizes, released in a different
order than they were taken, sweeping and retiring each round. The assertion is
not "fragmentation is low" but the strongest one available: after the run the
arena is **one hole**, and `largest()` equals the whole arena again. Out-of-order
release of unequal sizes is precisely the pattern that shreds a naive free list,
and the arena comes back whole.

The residual honest risk is the pattern coalescing cannot fix: a very long-lived
extent allocated in the middle of a busy arena pins a boundary forever. First
fit's low-address packing makes this less likely (long-lived setup allocations
happen early, so they land low), and the mitigation if it ever bites is
placement policy — allocate known-permanent extents from the top of the arena
downward — not compaction.

## Compaction: the answer is no, and why that is safe

Compaction means moving a live extent to a different address to close a gap.
Molt will not do it, and the reason is not difficulty, it is that compaction
contradicts the product:

- A pointer to mapped data can be stored *in* mapped data. That is the whole
  point of a single address space, and it is why porting existing code works.
  Moving an extent would require rewriting every pointer into it, and nothing in
  the system knows where those pointers are.
- Two domains hold the same address for the same object. Moving it would have to
  be atomic across every holder, on every hart, with every holder stopped.
- An in-flight DMA descriptor names an address the IOMMU has been told about.

So fragmentation must be *prevented*, and the prevention is the four mechanisms
above: classes so requests do not interleave at incompatible alignments,
coalescing so adjacent frees become one range, first fit so the top of each
arena stays clean, and quarantine so nothing is reused early. When those are not
enough, the allocator returns `Error::Exhausted`, which is a resource error the
caller can report — a mapping refused, not a mapping silently made slow.

## The arithmetic: how much there is

`Space::bounds(bits)` takes the top quarter of the lower canonical half, which is
one eighth of the space: everything below stays with the kernel's identity map
and, on RISC-V, the device window at 128 GiB (`the_narrowest_mode_clears_the_device_window`).

| Mode | Handed out | Page arena | Mega arena | Giga arena | 100 GiB mappings |
| --- | --- | --- | --- | --- | --- |
| Sv57 | 16 PiB | 4 PiB | 4 PiB | 8 PiB | 83 886 |
| Sv48 | 32 TiB | 8 TiB | 8 TiB | 16 TiB | 163 |
| Sv39 | 64 GiB | 16 GiB | 16 GiB | 32 GiB | 0 |

Read the last two rows as the real constraint they are: **tier 2 at data-analysis
scale needs Sv48 or wider**, and QEMU's `virt` and every board in
[`docs/hardware.md`](hardware.md) with an MMU worth the name report Sv48 or
Sv57 — which is why the widest-mode probe shipped first. An Sv39-only hart still
boots and still runs tiers 0 and 1; it just cannot host the log analyzer.

For the other direction: the gigabyte arena at Sv57 holds 8 388 608 gigabyte
leaves, and the megabyte arena holds 2 147 483 648 megabyte leaves. A system
would have to create and destroy a megabyte-class mapping every millisecond for
68 years to wrap the arena *without any reuse at all* — and reuse is the normal
case.

## The storage budget

One `Hole` is three `u64`s: 24 bytes. `Space::over` splits the slice evenly
between the three classes.

An arena in the steady state holds at most **one hole per live extent, plus
one** — that is the worst case, reached when every second extent is freed and
none of the frees are adjacent. So the slot count is a direct statement about
how many mappings of a class may coexist in the fragmented case:

| Slots per class | Bytes, all three | Fragmented extents per class |
| --- | --- | --- |
| 16 | 1 152 | 15 |
| 64 | 4 608 | 63 |
| 256 | 18 432 | 255 |

`[Hole::EMPTY; 192]` — 64 per class, 4.6 KiB — is the size a kernel that maps
some dozens of extents per class wants; a heavier system passes a bigger slice
and pays 24 bytes per slot.

Running out of slots is the one case where a *release* can fail, and the
behaviour is the conservative one: `Error::Full`, and the extent is not
recorded, which leaks the range rather than corrupting the free list.
`a_full_free_list_refuses_rather_than_loses_the_range` asserts both halves —
the error, and that the free list is unchanged afterward. The alternative
(silently merging non-adjacent ranges to make room) would hand out addresses
that were never freed, which is a correctness bug traded for a resource bug.
Leaking is also detectable: `free() + quarantined() + in use` no longer sums to
the arena.

## What the tests actually claim

| Test | Claim |
| --- | --- |
| `every_class_hands_out_its_own_alignment` | a class's extents start on its granule, always |
| `class_arenas_do_not_overlap` | the cut leaves no unowned gap and no shared range |
| `a_hundred_gigabyte_mapping_fits_in_one_extent` | 100 leaves, not 26 214 400 pages |
| `a_size_that_is_not_whole_leaves_rounds_up` | a byte past a leaf takes the next leaf |
| `released_addresses_wait_for_the_shootdown` | no reuse before a flush, ever |
| `retiring_the_swept_epoch_returns_the_addresses` | and full reuse after one |
| `an_epoch_that_was_never_swept_frees_nothing` | the flush cannot be shortcut |
| `churn_leaves_no_permanent_fragmentation` | out-of-order churn returns the arena whole |
| `an_exhausted_class_does_not_borrow_from_another` | exhaustion is local and loud |
| `a_full_free_list_refuses_rather_than_loses_the_range` | slots run out safely |
| `a_range_that_is_already_free_is_refused` | double release is caught, not doubled |
| `a_space_too_narrow_to_cut_is_refused` | Sv39 works, narrower is refused |
| `the_narrowest_mode_clears_the_device_window` | the handed-out range never collides with devices |
| `zero_bytes_name_no_page` | the degenerate request is an error |

Two of these exist because they caught real bugs during development, not because
they were foreseen: the epoch-coalescing collapse described above, and an
earlier `bounds` that put the Sv39 range on top of `paging::DEVICE_REGION`.

## Concurrency

A `Space` is `&mut`-driven and holds no interior mutability, so a kernel that
shares one wraps it in exactly one lock. That is defensible only because of the
churn argument: allocation happens at mapping creation, not per message, per
page, or per grant, so the lock is not on any hot path. `Extent` is non-`Copy`
and `#[must_use]` for the same reason `Frames` is — a dropped extent is address
space nobody will ever hand out again, and the type system is the cheapest place
to catch that.

`retire` is the only operation with a cross-hart precondition, and it is stated
as an argument rather than assumed: the caller passes the epoch every hart has
flushed through, which is the same shape as the shootdown protocol in
[`docs/smp.md`](smp.md).

## What is not done yet

- **No single one of it.** Stage 5.0 wired the consumers — the addresses in
  `MOLT_GRANT_OK` and `MOLT_FILE_MAP_OK` are cut here and mapped by
  [`grant`](../crates/molt-arch/src/lib.rs) — but each does it over a `Space` of
  its own. One machine-wide instance is what the uniqueness claim actually
  needs, and it arrives with the thing that would share it.
- **`O(holes)` search.** Fine at 64 slots, wrong at 64 000. If a real workload
  ever needs the latter, the fix is a size-indexed structure *inside* an arena;
  the `Space` interface does not change.
- **No NUMA and no hart affinity.** One address space, one pool. The magazines
  described above are where that starts.
- **Nothing reclaims a leaked extent.** There is no scan, no GC, and no process
  exit to fall back on; domain teardown must release what it allocated, which is
  a capability-lifetime problem rather than an allocator one.

## The decision, restated

- **Three arenas, one per page-table leaf level.** Alignment is an invariant,
  not a search.
- **Address-ordered first fit with immediate coalescing**, the same policy the
  heap already uses.
- **A freed address waits for a shootdown**, batched by epoch, and coalesces
  only with neighbours waiting on the same one.
- **No compaction, ever** — a global address is the product, so fragmentation is
  prevented rather than repaired, and exhaustion is a reported error.
- **Caller-supplied storage**, 24 bytes a hole, no allocator underneath.
- **Grant and revoke never allocate**, which is why a churning system does not
  fragment and why the single lock is not a bottleneck.
