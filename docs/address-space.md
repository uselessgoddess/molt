# The address space

[`docs/abi.md`](abi.md) picked LFI, and LFI's sandbox is 4 GiB wide. The
question this document answers is what happens when a program wants more than
that — a mapped file measured in terabytes, an index that does not fit, a
workload whose author is not willing to split it into pieces to make it fit.

The requirement, stated as it was given: mapping gigantic files must be native
and fast — faster than Linux, not merely competitive; existing code must port
without being re-architected into cooperating processes; and none of it may cost
safety, meaning a program still cannot touch the kernel and cannot touch another
program without being granted the right. Molt's "no processes" thesis is
explicitly negotiable if the thesis is what stands in the way.

The answer is that the 4 GiB stays, and stops being the ceiling. Molt gets
**three tiers inside one address space**, and which tier a program runs in is a
build-time decision, not a rewrite.

## What actually costs 4 GiB, and what does not

**The 4 GiB is the width of a zero-extension, not a policy.** LFI-RISCV holds
the sandbox base in `x21`, and the only instructions permitted to write the
three address registers are `add.uw x18, xN, x21`, `add.uw ra, xN, x21`, and
`add.uw sp, xN, x21` ([`riscv/verifier.tex`][spec], "Register Accesses").
`add.uw` zero-extends its first operand to 64 bits, so the reachable set is
`[base, base + 2^32)` by construction. Nothing in the design cares that the
number is four gibibytes; it cares that the hardware has a single instruction
that clamps a register to a power-of-two window for free.

**The guard cost is architecture-specific, and Molt's own docs got it wrong.**
The spec's x86-64 runtime chapter requires "The 40GiB memory region following
the sandbox must be unmapped" and the same before it, and concludes: "The
virtual memory layout allows one sandbox to be allocated for every 44GiB of
virtual memory. This allows for up to 2,977 sandboxes to be allocated within a
standard 47-bit x86-64 userspace" ([`x64/runtime.tex`][spec]). Arm64 is a
different number entirely — 80 KiB of guard in dense mode, "up to 65,535
sandboxes … within a standard 48-bit Arm64 userspace", or 4 GiB of guard each
side in sparse mode for 32,767 ([`arm64/runtime.tex`][spec]).

The upstream spec has **no RISC-V runtime chapter at all** — `riscv/` contains
`verifier.tex` and nothing else — so there is no published RISC-V guard figure
to quote, and the 44 GiB one is a statement about x86-64 addressing modes, which
allow a scaled index and a 32-bit displacement. RISC-V does not have those. A
verified memory operand is `N(x18)` or `N(sp)`, and `N` is an I-type or S-type
signed 12-bit immediate: ±2 KiB. The widest producible address is therefore
`base + 2^32 + 2047 + 7`, and the lowest `base - 2048`. **One page of guard on
each side covers every address a verified RISC-V program can form** — call it a
2 MiB megapage per side and stop thinking about it, which is 4 GiB + 4 MiB per
sandbox rather than 44 GiB.

[`docs/abi.md`](abi.md) applied the x86-64 figure to riscv64 and concluded
"five sandboxes, give or take, is what Sv39 has room for". That paragraph is
corrected there; the arithmetic above is the reason.

**What binds, then, is total address space** — and until this branch, Molt's
RISC-V port implemented Sv39 and only Sv39: 512 GiB, with device windows at
128 GiB. That is now fixed in code rather than in a plan.

## What shipped with this document

`crates/platforms/riscv/src/paging.rs` builds its tree three levels deep, as it
always did, and then roots it as deep as the hart allows. Each level above Sv39
costs one extra table whose entry 0 points at the root below, so two frames buy
the option on both Sv48 and Sv57. The probe is the privileged spec's own rule:
"If `satp` is written with an unsupported MODE, the entire write has no effect;
no fields in `satp` are modified" — so `enable` writes the widest mode, reads
`satp` back, and keeps the first one that sticks.

The result is asserted, not reported: `xtask` requires `MOLT_SATP_MODE: sv57` in
the riscv64 boot markers, and `verify_owned_mapping` now writes and reads back
its probe value at `1 << 54` — 16 PiB, an address no Sv39 hart can translate —
so the existing `MOLT_MAPPING_OK` marker is evidence of the widening rather than
a claim about it. QEMU's default rv64 CPU declares `max_satp_mode =
VM_1_10_SV57`, and `legalize_xatp` returns the old value when `validate_vm`
fails, so a hart that only did Sv39 would fail the marker instead of silently
narrowing.

512 GiB became 128 PiB — 2^39 to 2^57, a factor of 262,144 — and it is Molt's
own code, which is what [`docs/abi.md`](abi.md) said the fix would be.

Two more subsystems shipped behind it, both by the same rule that a width is
probed rather than declared. `Platform::address_space` answers with the address
bits *and* the tag bits the hardware admitted to: on RISC-V the second half is a
live probe of `satp`'s ASID field, which the specification leaves WARL and
UNSPECIFIED in width, so the kernel writes sixteen ones and counts what stayed;
on x86_64 both halves are readable without probing (`CR4.LA57` for the mode the
loader left on, `CPUID.01H:ECX[17]` for whether PCIDs exist at all). And the
global VA allocator of [`docs/va-allocator.md`](va-allocator.md) is now cut from
that probed width inside a booted kernel rather than from a constant. The two
markers are below, and both print numbers that came from the machine.

## The candidates

Every option that was actually considered, with the reason it is or is not the
answer. The requirement that kills most of them is not safety and not speed — it
is *"porting existing code must not mean splitting it"*.

| Option | Reach | Verdict |
| --- | --- | --- |
| Window the file inside 4 GiB by hand | 4 GiB live | **No.** This is the failure mode the requirement names. |
| Wider LFI window (`--p2size=variable`) | ≤ 2^n, n unproven | **Not the answer, worth measuring.** |
| 64-bit SFI by explicit masking | full | **No.** Pays an instruction per access to get what `add.uw` gives free. |
| wasm64 with bounds checks | full | **No.** Loses the guard-page trick and adds a toolchain. |
| eBPF-style whole-program verification | full | **No.** The verifier is the product, and it does not scale to applications. |
| CHERI | full | **Right answer, wrong decade.** No hardware in Molt's class. |
| Intel MPK / PKS | full | **Adopted as an x86_64 accelerator**, not as the mechanism. |
| RISC-V pointer masking (Ssnpm/Smnpm/Smmpm) | — | **Later.** Narrows tags; does not widen the window. |
| One page table per program (processes) | full | **Rejected as the default.** Costs pointer stability. |
| **Single address space, many views (SASOS)** | full | **Chosen.** See below. |

**Window it by hand.** The program keeps a 4 GiB aperture and maps pieces of the
file through it. It works, it is what every 32-bit program did, and it is
exactly "change your architecture so it fits". Rejected on the stated
requirement, not on merit.

**A wider LFI window.** `lfi-rewrite` accepts `--p2size=variable` alongside
`--p2size=32`, so a different power-of-two window is something the toolchain
contemplates, and liblfi's own README says it "currently only supports 4GiB
sandboxes, although this may change". But a wider window is still a window; 2^40
does not map a file that does not fit in 2^40, and every widening moves guard
cost and emitted-code cost in ways the upstream documents do not quantify. This
raises the tier-1 ceiling if it ever lands. It is not the tier-2 answer.

**64-bit SFI by explicit masking.** Replace `add.uw` with `and` against a mask
register and the window can be any power of two up to the whole space. The cost
is that the clamp stops being free: `add.uw` folds the base add and the
truncation into the one instruction the address computation needed anyway, and a
mask-then-add is strictly more work on the hot path of every load and store.
Molt would be paying a per-access tax on all programs to raise a ceiling that
only some programs reach.

**wasm64.** WebAssembly's `memory64` is the closest thing to a shipping answer
to this exact question, and its own experience is the argument against it: with
a 64-bit index space the 8 GiB guard region that makes wasm32 accesses free no
longer covers the address space, so implementations fall back to explicit bounds
checks and the performance gap against wasm32 is the well-known result. On top
of that it is a second compilation target and a second ABI for code that Molt
already compiles natively. Molt would be adopting a slower isolation mechanism
to get the reach that a page table gives it for nothing.

**eBPF-style verification.** Prove memory safety of the whole program at load
and no runtime check is needed at all. This is a beautiful answer for a hundred
instructions of packet filter and an unworkable one for an application: the
verifier becomes the language definition, and every program that cannot be
proven has to be rewritten until it can. That is the porting requirement failing
in a more expensive way.

**CHERI.** Architecturally this is the right answer to the question as asked:
128-bit capabilities carry bounds and permissions in the pointer, so a 64-bit
address space needs no window, no guard, and no page-table switch to be safe,
and sharing a pointer between domains is sharing a pointer. The problem is
entirely hardware. Morello is an Arm research prototype; CHERIoT is a 32-bit
microcontroller design; no board in the class [`docs/hardware.md`](hardware.md)
surveyed has any of it. This stays as the branch to take if CHERI-RISC-V
silicon ever reaches the $30–$70 bracket, and the tiering below is what makes
taking it a driver-level change rather than a redesign.

**Intel MPK / PKS.** Sixteen protection keys, a register that says what the
current key set may do, and a userspace instruction to change it — permission
switching inside one address space without touching CR3 or the TLB. This is
genuinely the mechanism tier 2 wants, and it is x86-only and capped at sixteen
domains, and it does not answer the width question at all (an MPK domain has the
same 47-bit reach the address space had). So: an acceleration of the chosen
design on x86_64, for the case where a domain switch is hot and the domain count
is small. Not the design.

**RISC-V pointer masking.** `Smmpm`/`Smnpm`/`Ssnpm`/`Supm`/`Sspm` are in the
1.13 privileged set and in QEMU master's `max` CPU — not in the 8.2.2 that CI
pins, and not in any board. They make the top address bits ignorable so software
can put tags there. Useful for a future capability representation. Irrelevant to
how much memory a program can reach.

**One page table per program.** The crude reading of "compromise the
principles". It reaches everything and it is what every mainstream OS does, and
the price is precisely the thing that makes porting hard again: the same virtual
address means different memory in different programs. Which means shared memory
is shared *offsets*, not shared pointers; a structure with an internal pointer
cannot be handed to another program without relocation; and the kernel cannot
dereference a submitted pointer, so it copies (`copy_from_user`) or pins
(`get_user_pages`). Molt would be buying reach by importing the two costs it was
built to avoid.

## The decision: one address space, three tiers

**Virtual addresses are globally unique. Views are not.** Every mapped object —
a heap, a file extent, a ring — is assigned an address from one global allocator
and lives at that address for everyone who can see it at all. What differs
between domains is not the address, it is whether the address is *present*. That
is the old single-address-space-OS idea (Opal, Mungi, Singularity's SIPs)
brought forward onto hardware that finally has the bits for it: 128 PiB, and
ASIDs so switching a view is not a TLB flush.

| | Tier 0 — cell | Tier 1 — aperture | Tier 2 — domain |
| --- | --- | --- | --- |
| Isolation | the compiler | LFI verifier + guard pages | page-table view + ASID |
| Reach | the whole kernel | 4 GiB | the whole 57-bit space |
| Entry cost | a call | a call | a `satp` write, no flush |
| Trusted | yes | no | no |
| For | drivers, the executor | small programs, hot IPC | big data, mapped files |

The three share one thing that makes the tiering real: **the same `molt-abi`
ring ABI**. A cell submits into a ring; an aperture submits into a ring at its
sandbox-relative address; a domain submits into a ring at its global address.
[`docs/userspace.md`](userspace.md) already claims "a cell can move from inside
the kernel to inside a sandbox, or back, as a build-time decision". This adds
one more destination to that sentence and keeps the claim testable, which is
what the marker table below is for.

So the "no processes" principle survives in the form that mattered — there is no
`fork`, no `exec`, no process table, no pid, and no per-program address space
*layout*. What is given up is the claim that there is only ever one page table.
A domain is a *view*, not a process: it does not rename memory, it hides it.

## Why this is faster than Linux, mechanically

Not "should be faster" — these are operations Linux performs that Molt's design
does not have to. The numbers belong to Stage 4.7, which is where this document
expects to be judged.

**A submitted pointer needs no copy and no pin.** Because the address is global,
the buffer a program names is at that address in the kernel's view too. The
kernel checks the pointer against the extent of the capability that submitted it
— one range check — and then uses it. Linux does `copy_from_user` for small
buffers and `get_user_pages` for large ones; the latter takes references on every
page, holds them for the life of the I/O, and is the reason long-lived
zero-copy in Linux needs pinning accounting at all. Molt's extents are already
resident and already IOMMU-mapped ([`kernel/src/isolation.rs`](../kernel/src/isolation.rs)),
so the device sees an address the domain was allowed to name and nothing else.

**Mapping a gigantic file is an extent, not a fault storm.** Linux's `mmap`
installs a VMA and then populates it on demand, one fault per page until
readahead or `MAP_POPULATE` catches up; a terabyte at 4 KiB is 268 million
faults' worth of work spread over the run. Molt installs the translation at map
time, at the largest leaf the alignment allows. `map_range` already emits 2 MiB
leaves when `Granularity::LargeOk` and the span permits, and level 2 is the same
code with `level == 2`: **a 1 TiB file is 1024 page-table entries and 1024 TLB
entries, not 268 million of either.** For a workload that walks a large file,
the difference is not the fault count, it is that the TLB stops missing.

**The page cache is the mapping.** A read costs two copies on a system with more
than one address space — once into the kernel's page cache, once out to wherever
the caller asked, because the caller's address for those bytes is not the
kernel's address for them. The second copy pays for the disagreement, not for the
file. Molt has no disagreement to pay for: a window of a file is read into frames
once, given an address out of the same allocator everything else uses, and that
address is what the window is called in every view that holds it. Handing it to a
second domain adds a leaf and moves no bytes, so `copy_from_user` has nothing to
do — the buffer is already at the address both ends name it by.
[`crates/molt-arch/src/cache.rs`](../crates/molt-arch/src/cache.rs) is the
bookkeeping and nothing more: which windows are cached, at which addresses, over
which frames, and how many views hold each. Reading the bytes in stays the
filesystem's, mapping them stays `Platform::grant`, and counting the leaves a
grant shares stays [`refcount`](../crates/molt-arch/src/refcount.rs). Eviction
hands the extent back out rather than dropping it, because the caller still owes
the unmap, the shootdown, and the retire, in that order.

**Sharing is sharing a pointer.** Two domains granted the same extent see it at
the same address, so a structure containing pointers can be handed over as-is.
This is the porting requirement met at the mechanism level: code written against
threads sharing a heap keeps working when the threads become domains, because
the thing that usually breaks — a pointer that means something else on the other
side — cannot happen. It is also why there is no serialization step on the IPC
path that a POSIX port would need.

**A domain switch does not flush.** ASID-tagged entries survive a `satp` write,
so crossing between domains costs the write and the pipeline effect, not a cold
TLB. And most crossings do not happen: the ring is shared memory, so submission
and completion are stores and loads, and the switch is only paid when control
actually has to move.

**What Linux still does better, honestly.** Its VA allocator, TLB shootdown, and
NUMA placement are decades of tuning against real workloads; a global VA space
has fragmentation and recycling problems a per-process space does not; and
128 PiB is a real ceiling that a per-process design does not have, because each
process gets its own copy of the space. Molt is trading a ceiling it will not
hit for costs it does not want to pay.

## Safety, which is not negotiated away

Five claims, in summary form. The adversary's side of each — what an attacker at
each tier holds, which mechanism takes it away, what is left over, and the six
rules a ring shared with a hostile domain has to obey — is
[`docs/threat-model.md`](threat-model.md), written before the code so the code
can be reviewed against it.

**The kernel is not reachable.** Kernel memory is *absent* from a domain's view,
not merely marked supervisor-only — no PTE, no address to speculate against, and
`SUM` stays clear so even a kernel bug cannot casually dereference a domain
address without meaning to.

**Another domain is not reachable by default.** A globally unique address is not
a globally *present* one. An extent appears in a second view only when a
`Capability<Region>` grant installs it, with its own rights, and revocation
unmaps it and shoots the TLB down before the grant is released. This is the same
capability shape [`docs/memory.md`](memory.md) already uses; it is not a second
handle space.

**Hostile code gets hardware, not a verifier.** Same-address-space isolation —
LFI's window, MPK's key register — is enforcement against a *program*, and both
are soft against speculative side channels in a way page tables and ASIDs are
not. That is a second reason tier 2 exists: code that is merely untrusted can
run in an aperture, and code that is actively hostile gets a domain. (The two
examples are not equally soft, and
[`docs/threat-model.md`](threat-model.md#tier-1-is-softer-but-be-precise-about-why)
says why the sturdier argument is about trusted computing bases rather than
about speculation.)

**Devices are already contained.** Stage 4.5/4.6 put every endpoint in its own
IOMMU domain, so widening what a program can address does not widen what a
device can address.

**The honest caveat.** A tier-0 cell containing `unsafe` can still corrupt
anything, because tier 0's isolation *is* the compiler. That is not new and it
is why the tiering is a build-time decision rather than a runtime one: code
whose trust level changes gets rebuilt into a lower tier, and the marker table
is how that is proven to still work.

## What this costs Molt, stated up front

Three things in the existing design have to change, and it is better to name
them here than to discover them.

**Refcounts on leaves, not on frames.** [`docs/memory.md`](memory.md)
deliberately omitted them — "there is no `mmap`, no copy-on-write, and no
sharing between address spaces, so the count would be 0 or 1". A granted extent
is exactly memory with two holders, so the count stops being 0 or 1 and the
omission stops being free. But the unit is the *leaf that was mapped*, not the
frame: a 1 GiB leaf is one entry installed and one entry revoked, so it is one
count, and 262 144 counts behind it would be 262 143 words of bookkeeping that
can never disagree with each other. That is exactly the waste Redox's `PageInfo`
survey warned about — "511 or in the extreme case 262,143 useless PageInfos" per
large page — met by keying the count on the level the mapping was made at
(4 KiB, 2 MiB, 1 GiB) rather than on the smallest unit the hardware has.

The consequence to accept up front: a shared 1 GiB leaf cannot be un-shared one
page at a time. Revoking a subrange of a granted extent means splitting the leaf
into the next level down first, which is a real operation with a real cost — and
it is a cost paid only by the caller who asks for the odd shape, instead of by
every mapping in the machine.

**A global VA allocator.** One authority, allocating extents rather than pages,
with alignment classes so a 1 GiB-mappable extent gets a 1 GiB-aligned address.
Fragmentation and address recycling become real problems that a per-process
design does not have. This is the piece the whole tiering rests on, so it has
its own design document — [`docs/va-allocator.md`](va-allocator.md) — covering
the policy, why buddy and slab were both rejected, why compaction is never an
option, and what happens to a freed address before it is reused. It is code and
tests today ([`crates/molt-arch/src/va.rs`](../crates/molt-arch/src/va.rs)) and
`MOLT_VA_OK` prints a live carve from a booted kernel.

**TLB shootdown and ASID lifetime.** Revocation must reach every hart that could
have cached the entry, and ASIDs are a finite tag space, so rollover means a
flush. Both are cross-hart protocols, which is Stage 4 machinery
([`docs/smp.md`](smp.md)), not new invention. The arithmetic is in
[the tag budget](#the-tag-budget-counted-not-assumed) below, and the one thing
worth correcting early is that **a tag is spent per domain, not per grant**: a
grant or a revoke changes what a view maps and costs a shootdown, and it does
not consume an ASID at all.

## The tag budget, counted not assumed

"ASIDs are finite" is not a design; a number is. The number is now measured on
both ports, and it decides how many domains can be resident at once and what a
rollover costs.

**What a tag is spent on.** One per *live domain*, and nothing else. It is worth
being explicit because the opposite is an easy assumption to make: a grant, a
revoke, a map, and an unmap all change what a view contains and all cost a
shootdown, and **none of them consumes a tag**. That is what makes the budget
tractable — the churny operations are exactly the ones that are not charged
against it.

**What the machine says.** `satp`'s ASID field is bits 59:44, so sixteen bits at
most on RV64, and the specification declines to say how many a hart implements:
the field is WARL and its width is UNSPECIFIED. So the kernel writes ones into
the whole field, reads back what stuck, and counts the low contiguous run —
[`Asid::width`](../crates/platforms/riscv/src/satp.rs). Only the *contiguous*
run counts, because WARL lets a hart keep a high bit it does not decode, and two
domains whose tags alias in the bits that do decode is worse than no tags at
all. QEMU's `virt` answers 16, so 65 535 concurrent domains with tag 0 reserved
for the kernel. x86_64's equivalent is PCID: 12 bits, 4 095 domains, on hardware
that has it — QEMU's default model does not, and there the honest answer is 0.

**What a rollover costs, on the workload that provokes it.** Take the one worth
worrying about: 32 harts, domains created and destroyed continuously. Tags are
handed out until the space wraps; the wrap bumps a generation and every hart
must flush, because there is no per-tag invalidate cheaper than the flush at
that point ([`crates/molt-arch/src/asid.rs`](../crates/molt-arch/src/asid.rs)).
So the cost is one all-hart flush per 65 535 domain creations. A workload that
creates a domain every microsecond — which would be extraordinary, since a
domain is a page table and a set of capabilities — reaches a rollover every
65 milliseconds and pays 32 cold TLBs for it. Anything realistic is orders of
magnitude below that, and the reason is the paragraph above: the operations a
busy system actually repeats are grants and revokes, and they cost zero tags.

**And when there are no tags at all.** A hart with a 0-bit field, which is what
the x86_64 smoke exercises today, flushes on every domain switch. That is slower
and exactly as safe: `Asids::assign` returns `Flush::Everything`, the switch
pays a cold TLB, and nothing about isolation changes. It is a performance floor,
not a correctness fork, and having both paths under CI on different ports is why
that can be stated rather than hoped.

`MOLT_ASID_OK: bits=16 domains=65535` on riscv64 and `bits=0 domains=0` on
x86_64 are the two ends of that range, printed by the kernel that will use them.

## Which tier a program actually gets

The tiering is only real if the assignment is obvious for concrete programs.
Three, which are the ones this design was argued against:

| Program | Tier | Why that one |
| --- | --- | --- |
| a `uutils` coreutil — `ls`, `wc`, `sort` on ordinary files | **1 — aperture** | its whole working set fits in 4 GiB with room to spare, and it starts and exits often enough that the cheap entry matters more than reach |
| a log analyzer with 100 GB mapped at once | **2 — domain** | the requirement *is* the reach; at tier 1 it would page through a window, which is the design being avoided |
| the NVMe driver | **0 — cell** | it is trusted by construction, needs the kernel's own structures, and an isolation boundary would buy nothing and cost a crossing per submission |

The claim that makes this a tiering rather than three unrelated mechanisms is
that all three speak the same `molt-abi` ring ABI, so the tier is a build-time
decision and the coreutil can be rebuilt as a domain — or the analyzer as a cell
— without touching its source. That is what `MOLT_TIER_PARITY_OK` is for: one
cell, built for all three, proving the same behaviour in each. Until that marker
exists, the table above is a plan; after it, it is a property.

## What ships instead, in order

| Step | Marker | What it is |
| --- | --- | --- |
| widest `satp` mode probed at boot | `MOLT_SATP_MODE: sv57` | shipped: 512 GiB → 128 PiB |
| a translation above 512 GiB, performed | `MOLT_MAPPING_OK` | shipped: probe at `1 << 54` |
| global VA allocator with extents | `MOLT_VA_OK` | shipped: 100 GiB in 100 leaves, from a probed width |
| the tag budget, read off the hardware | `MOLT_ASID_OK` | shipped: 65 535 domains on riscv64, 0 on x86_64 |
| 1 GiB and 2 MiB leaves on demand | `MOLT_HUGE_MAP_OK` | shipped: one PTE per gibibyte, not 262,144 |
| a freed range held until every core flushed | `MOLT_SHOOTDOWN_OK` | shipped: the order the rest of this table rests on |
| refcounts on the mapped leaf, not the frame | `MOLT_REFCOUNT_OK` | shipped: 100 GiB in 100 records, not 26 million |
| a file mapped as an extent, read at its address | `MOLT_FILE_MAP_OK` | shipped: one window, two domains, one device read, no copy |
| a second view with its own ASID | `MOLT_DOMAIN_OK` | shipped: tier 2 exists, with no kernel leaf in it |
| a fault in a domain that stays there | `MOLT_DOMAIN_FAULT_OK` | the view is a boundary — needs the switch Stage 5.1 brings |
| grant and revoke of an extent between domains | `MOLT_GRANT_OK`, `MOLT_REVOKE_OK` | shipped: sharing needs a right, and taking it back needs an order |
| an aperture inside a domain | `MOLT_SANDBOX_OK` | tier 1 nests in tier 2 |
| the same cell built for all three tiers | `MOLT_TIER_PARITY_OK` | the claim, made testable |

## The decision, restated

- **The 4 GiB stays** for tier 1, where it is free, and stops being the ceiling
  for anything else.
- **The address space is single and global**; domains differ in what is present,
  never in what an address means.
- **A domain is a view, not a process.** No `fork`, no `exec`, no pid, no
  per-program layout — the parts of the thesis that were load-bearing survive;
  the "exactly one page table" part does not.
- **Reach comes from hardware**, not from a wider software window: Sv57 today,
  CHERI if it ever ships, MPK as an x86_64 accelerator.
- **The tier is a build-time decision**, because all three speak the same
  `molt-abi` rings — and `MOLT_TIER_PARITY_OK` is how that stops being a claim.

[spec]: https://github.com/lfi-project/lfi-spec
