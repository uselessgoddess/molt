# The tier-2 threat model

[`docs/address-space.md`](address-space.md) put three tiers in one address space
and spent five paragraphs on safety, written as claims. This document is the
other side of them: what an attacker at each tier actually holds, which
mechanism takes each attack away, and — the part that matters more — what is
*not* taken away and must therefore be said out loud rather than discovered.

It exists because "there is no PTE for it" is a true sentence that answers only
one question. A domain is a view in a shared address space; the interesting
attacks are not against the absent mappings but against the things that are
deliberately *present*: the rings both sides write, the tags the TLB keys on,
the addresses the kernel is handed and must check.

## The questions, answered first

| Question | Answer | Section |
| --- | --- | --- |
| What isolates a domain, beyond the missing PTE? | Four mechanisms, and absence is only the first | [mechanisms](#four-mechanisms-and-what-each-one-actually-stops) |
| Can a hostile domain tamper with the rings in the global VA? | **With today's `SpscRing`, yes — into kernel UB.** The cross-domain ring must be a different type, and here are its six rules | [rings](#the-rings-are-the-real-attack-surface) |
| Are addresses secret? | **No, and nothing may ever depend on their being secret.** Enforcement is the PTE, never obscurity | [uniqueness](#2-uniqueness-is-not-presence-and-an-address-is-not-a-secret) |
| Speculative side channels? | Meltdown-class: gone by construction. Spectre-v1 in the kernel's own checks: live, with a named fix. v2/MDS: unaddressed, and named | [speculation](#speculation-what-is-defeated-by-construction-and-what-is-not) |
| Is tier 1 really "softer" than tier 2? | Yes, but not for the reason a one-line summary suggests | [tier 1](#tier-1-is-softer-but-be-precise-about-why) |
| Denial of service? | **Not guaranteed.** No preemption, no quotas yet | [availability](#availability-is-not-promised) |
| Stale tags after revoke? | A generation check plus the shootdown the VA allocator already waits for | [tags](#3-a-tag-is-not-authority) |
| Devices? | Already contained, one IOMMU domain per endpoint, shipped in Stage 4.5/4.6 | [devices](#4-a-device-cannot-be-used-as-a-proxy) |

## The attacker, stated concretely

The interesting attacker is **code Molt chose to run and does not trust**: a
downloaded program, a decoder fed hostile input, a cell whose author is not the
kernel's author. It is not a physical attacker, not a malicious firmware, and
not a compromised build of the kernel itself — those defeat everything below and
saying so is more useful than pretending otherwise.

| | Tier 0 — cell | Tier 1 — aperture | Tier 2 — domain |
| --- | --- | --- | --- |
| Instructions it may execute | any | any the verifier accepts | any |
| Addresses it can name | all of them | its 4 GiB window | all 57 bits |
| Addresses it can *reach* | all of them | its window | what its view maps |
| Isolated by | the compiler | Molt's verifier + guard pages | the MMU |
| Trusted computing base for that isolation | rustc, and every `unsafe` in the cell | rustc, the verifier, the loader's W^X ordering | the MMU, the shootdown protocol, the ring validator |
| A single bug in that TCB costs | everything | everything | one mechanism, usually |

That last row is the whole argument for tier 2 and the honest reason to prefer
it for hostile code: not that page tables are magic, but that tier 0 and tier 1
each have one artefact whose correctness *is* the isolation, and tier 2's
isolation is a hardware mechanism that is wrong in known, enumerable ways.

**Assumed true, and load-bearing:** the hart implements its own privileged
specification; the boot chain that loaded the kernel is trusted; `molt-abi`'s
layouts are what both sides think they are (asserted by host tests, per
[`docs/abi.md`](abi.md)); and the kernel's own code is not attacker-supplied.

## Four mechanisms, and what each one actually stops

### 1. Absence, not permission

A domain's root table does not contain the kernel. Not "contains it with the
user bit clear" — *does not contain it*. The distinction is the entire Meltdown
family: a supervisor-only PTE is a translation that exists, and a translation
that exists can be walked speculatively and its data forwarded to a transient
instruction that leaks it. A missing translation forwards nothing, because
there is nothing to forward.

Two consequences that future code must respect, because both are easy to break
by accident:

- **The physmap goes too.** The kernel maps all of usable RAM to read frames it
  owns (`MOLT_PHYSMAP_OK`, [`docs/memory.md`](memory.md)). A domain view that
  inherits that mapping has every other domain's memory in it, and the tiering
  is over. The domain's tree is built for the domain, not cloned from the
  kernel's.
- **`SUM` stays clear** (`sstatus.SUM` on RISC-V, SMAP on x86_64), so the
  reverse direction — the kernel casually dereferencing a domain address it was
  handed — traps instead of working. This matters more than it sounds: it turns
  "the kernel forgot to validate a pointer" from a silent cross-domain read into
  a fault. It is a backstop for the validation in the [ring rules](#the-rings-are-the-real-attack-surface),
  not a substitute for it.

This is checkable rather than assertable, and the checker already exists:
`Audit::cover` / `Audit::accepts` ([`docs/testing.md`](testing.md)) walks *live*
page tables. Pointed at a domain's tree it answers "is any kernel leaf present
here" directly — that is `MOLT_DOMAIN_ABSENT_OK` below.

### 2. Uniqueness is not presence, and an address is not a secret

A single address space means every extent has one global address. It is
tempting — and wrong — to let that shade into "an attacker would have to guess
where it is". **Molt does not get to make that argument.** In a SASOS, an
address that leaks through any channel (a returned pointer, a timing
difference, a debug print, a granted structure containing pointers) is
permanently and globally meaningful. There is no per-process randomisation to
re-roll, because there is no per-process space.

So the design states the strong form: **knowing an address must be worth
nothing.** Enforcement is the absent PTE, every time. Anything that would
protect an extent by making it hard to find is not a protection, and reviewing
future code means asking "does this still hold if the attacker knows every
address in the machine?"

The corollary is a small, real discipline: the boot markers already print
addresses (`MOLT_VA_OK: … at 0xe0000000000000`), and that has to stay
harmless. It is, under this rule.

### 3. A tag is not authority

An ASID keys TLB entries so a domain switch is a `satp` write and not a flush.
It does not grant anything: two views with the same tag would alias, which is
why tags are never reused without the generation check in
[`crates/molt-arch/src/asid.rs`](../crates/molt-arch/src/asid.rs). `Asids::live`
compares the generation a tag was issued in against the current one; a tag from
a previous generation is dead, and `assign` returns `Flush::Everything` on the
wrap that makes it so.

The attack this closes: a domain is torn down, its tag is handed to a new
domain, and a hart still holds TLB entries from the old one. The new domain
would then read the old domain's memory with no fault, no PTE consulted, and no
trace. Two things stop it, and both are needed — the generation check (a stale
`Asid` cannot be presented as live) and the full flush on rollover (the hardware
has no per-tag invalidate that is cheaper than the flush at that point).

The budget is measured, not assumed: `MOLT_ASID_OK` prints what the hart
implements, which is 16 bits / 65 535 concurrent domains on QEMU's `virt`, and 0
bits / 0 domains on QEMU's default x86_64 model — where the correct behaviour is
to flush on every switch, and does. That "0 domains" path is not a degraded
security posture; it is the same posture, paid for in TLB misses.

### 4. A device cannot be used as a proxy

The oldest way around an MMU is to ask something else to do the write. Stage
4.5/4.6 put every endpoint in its own IOMMU domain
([`kernel/src/isolation.rs`](../kernel/src/isolation.rs)), so a domain that can
submit I/O still cannot name memory outside the extent its capability covers —
the device is programmed with addresses the kernel checked, and the IOMMU
refuses everything else. Widening what a *program* can address did not widen
what a *device* can address, which is the property that had to survive this
design and does.

## The rings are the real attack surface

The mechanisms above are about memory the attacker cannot touch. The ring is
memory it is *supposed* to touch, shared with the kernel by construction, and it
is where a design review earns its keep.

### What today's ring does under a lying producer

[`crates/molt-core/src/ring.rs`](../crates/molt-core/src/ring.rs) is an SPSC
ring whose consumer end reads:

```rust
let head = self.head.load(Ordering::Relaxed);
let tail = self.tail.load(Ordering::Acquire);
if head == tail {
    return None;
}

let value = self.slots[head % N].with(|slot| unsafe { (*slot).assume_init_read() });
```

Its `// SAFETY:` comment is exactly right about the world it was written for:
"the producer's release published a fully initialized value". Both endpoints are
in-kernel and mutually trusting, `split` hands out one of each, and the contract
holds.

Put the same structure in memory a hostile domain can write and every clause of
that comment becomes a request:

- **A tail that was never earned.** The producer stores `tail = head + 1` without
  ever writing the slot. `head % N` is in bounds, so there is no out-of-bounds
  read — and that is the trap, because the bug is not spatial. `assume_init_read`
  on never-written memory is UB, and for any `T` with a validity invariant (an
  enum discriminant, a `NonNull`, a `bool`) it is UB the compiler is entitled to
  act on, not merely a garbage value.
- **A tail that rewinds.** Store `tail = head + 1`, let the kernel consume, store
  it again. `assume_init_read` *moves out* of the slot; reading the same slot
  twice duplicates whatever it held. Today's `Op` is plain data so this is
  latent rather than live — which is a fact about the current payload, not a
  property of the design, and it stops being true the first time an operation
  carries something that owns a resource.
- **A tail arbitrarily far ahead.** `tail - head > N` makes the kernel drain
  slots the producer never touched, in a loop, at attacker-chosen length.
- **A field that changes between the check and the use.** The kernel range-checks
  a submitted `Region` against the capability's extent and then uses it. If both
  reads come from the shared page, the attacker rewrites it in between — the
  double-fetch bug, and the one range check that
  [`docs/address-space.md`](address-space.md) sells as the performance win is
  precisely where it lands.

None of this is a defect in `SpscRing`. It is a type whose safety contract names
a single trusted producer, used outside that contract.

### The six rules for the cross-domain ring

So the decision is: **`SpscRing` stays what it is, and the cross-domain ring is
a different type in `molt-abi`.** Not a wrapper with checks bolted on — a
separate implementation whose consumer end assumes nothing, because the
difference between the two is not a flag, it is who is allowed to be wrong.

1. **The consumer's index is kernel-private.** The kernel keeps its own `head`
   for the submission ring in kernel memory and never reads the shared copy as
   truth. The shared copy is published for the producer's backpressure only, and
   a domain that corrupts it starves itself.
2. **The producer's index is validated, not trusted.** Read `tail` once;
   `tail.wrapping_sub(head)` must be `<= N`. Anything else is a protocol fault —
   which already has a home, `fault(kind, pc)` in [`docs/abi.md`](abi.md)'s
   four-entry call table — and not a panic, because a hostile domain must not be
   able to take the kernel down by lying.
3. **The payload has no invalid bit patterns.** No `assume_init_read` on shared
   memory, ever. The slot's bytes are copied into a kernel-local buffer and then
   *parsed*: the `u32` tag `docs/abi.md` already specifies is matched, an unknown
   tag is a rejected submission, and the arms are fixed-size POD. A parse that
   can fail is the point; `assume_init_read` is a parse that cannot.
4. **Read once.** Everything the kernel decides with comes from the local copy.
   No field is re-read from the shared page after it has been checked, which is
   the double-fetch closed by construction rather than by care.
5. **One range check, on the copy, against the submitting capability's extent.**
   That is the fast path address-space.md promises, and it is only sound under
   rule 4. It is a check of a value the domain cannot change any more.
6. **The rings live in the domain's own extent.** Everything the kernel writes
   back — completions, results — goes into memory the domain could have written
   itself, so corrupting it is self-harm and never a way to reach a third party.

Rules 1 and 4 are the ones that carry the weight; 2, 3, 5, 6 are cheap once
those two are true. The cost is one copy of a submission-sized struct per
operation, which is bounded, on a path that already touches the cache line.

`MOLT_RING_FAULT_OK` below is the marker that turns all six from a document into
a thing that happened: a domain publishes a tail it did not earn, the kernel
faults it, and the boot continues.

## Speculation: what is defeated by construction, and what is not

**Meltdown-class (reading through a permission bit): gone**, and gone for a
structural reason rather than a mitigation — [absence, not
permission](#1-absence-not-permission). There is no supervisor-only mapping in a
domain's view to speculate against. This is the one place where the SASOS design
is *stronger* than a conventional one, since KPTI is a retrofit of exactly this
property at considerable cost.

**Spectre-v1 in the kernel's own validation: live, and this is the real one.**
Rule 5 above is a bounds check on an attacker-controlled index, immediately
followed by a use — the textbook gadget. The architectural result is correct;
the transient one can touch memory outside the extent and leave a cache trace.
The fix is not a fence, it is masking: force the out-of-range case to produce an
in-range address (Linux's `array_index_nospec` shape), which needs no
architectural barrier and therefore works on both ports. That matters here,
because x86_64 has `lfence` and RISC-V has no ratified speculation barrier to
reach for. Masking is cheap, portable, and belongs in the validator itself
rather than at its call sites.

**Spectre-v2 / branch-target injection across domains: unaddressed.** A domain
switch is a `satp` write; it does not flush branch predictors, and that is
exactly why the switch is fast. Two mutually hostile domains on one hart can
train each other's indirect branches. x86_64 has IBPB to pay for on switch;
RISC-V has no standard equivalent. The honest position is that Molt does not
defend against this today, and that the first defence — if one is ever needed —
is scheduling, not a barrier.

**MDS / L1TF / sibling-hart sampling: unaddressed, and cheap to constrain now.**
These leak across SMT siblings sharing a core's buffers. The mitigation that
costs Molt nothing to *state* today is a scheduling rule: do not co-schedule two
domains on sibling harts. Molt's executor is the thing that would enforce it,
and writing the rule down before the executor grows a policy is the entire
reason this section exists.

**Cache and timing channels between domains: intrinsic and not claimed.** Two
domains sharing a machine share caches, TLBs, memory bandwidth, and a clock.
Molt does not offer confidentiality against a co-resident attacker willing to
measure, and a grant is consent to exactly the sharing it names. Saying this
plainly is worth more than a mitigation list that would not survive contact with
a real measurement.

## Tier 1 is softer, but be precise about why

[`docs/address-space.md`](address-space.md) says same-address-space isolation is
"soft against speculative side channels in a way page tables and ASIDs are not".
That is the right summary and it is worth two sentences of precision, because
the two examples it groups are not equally soft.

MPK's key register is checked *at* the load, and Meltdown-PK showed the check
can be bypassed transiently — the data comes back. LFI's window is different in
kind: the address is produced by `add.uw x18, xN, x21`, so being in-window is a
*data dependency* of the address itself, not a predicate a predictor can guess
past. Value speculation on `x21` is what an escape would need, and mainstream
cores do not do it. So tier 1 is not "MPK with different letters".

Tier 2 is still the answer for hostile code, for the reason in the attacker
table: tier 1's isolation is the correctness of a verifier Molt intends to write
itself ([`docs/abi.md`](abi.md) records that as an honest cost), and one bug
there is a total escape with nothing underneath it. Page tables have known
failure modes and a hardware mechanism enforcing them. That is a statement about
trusted computing bases, not about speculation, and it is the sturdier reason.

## Availability is not promised

Confidentiality and integrity are defended above. Availability is not, and the
useful thing is to enumerate rather than hedge:

- **No preemption.** [`docs/abi.md`](abi.md) already says user programs are
  cooperative and there is no timer interrupt into sandbox code. A domain that
  spins holds its hart. This is a known, recorded gap with a known price
  (instrumented metering), not an oversight.
- **No per-domain quotas.** A domain can consume VA extents, frames, and
  capability slots up to the machine's supply. Exhaustion is at least *loud* —
  `va::Error::Exhausted` is a reported error with a number attached, per
  [`docs/va-allocator.md`](va-allocator.md) — but "loud" is not "contained".
  Quotas are a capability-lifetime feature and are named in that document's
  not-done list too.
- **Tag churn is an amplifier, and a weak one.** Creating and destroying domains
  rolls the ASID generation, and each rollover costs every hart a flush. The
  arithmetic bounds the attack: 65 535 assignments per rollover on a 16-bit
  hart, so the attacker pays five orders of magnitude more than the victim. On a
  0-bit hart every switch flushes anyway, so there is nothing to amplify.
- **Ring backpressure is not DoS.** A full ring is `N` slots and the producer
  stalls. A domain that fills its own ring stalls itself.
- **Quarantine is bounded by the sweep.** Freed addresses wait for a shootdown,
  and a domain cannot extend that wait, because `sweep`/`retire` are kernel-driven.

## What proves any of this

The same rule as everywhere else in this kernel: a claim that no marker covers
is a claim, and [`docs/testing.md`](testing.md) is why that is the standard.

| Marker | The claim it turns into evidence | Status |
| --- | --- | --- |
| `MOLT_ASID_OK` | the tag budget is what the hart implements, counted, not assumed | **shipped** |
| `MOLT_VA_OK` | a freed address is not reissued before its epoch is retired | **shipped** |
| `MOLT_DOMAIN_OK` | a second view exists with its own tag | planned |
| `MOLT_DOMAIN_ABSENT_OK` | `Audit` walks a domain's live tree and finds no kernel leaf — including the physmap | planned |
| `MOLT_DOMAIN_FAULT_OK` | a domain touching an absent address faults, and the fault stays in the domain | planned |
| `MOLT_RING_FAULT_OK` | a published tail the producer never earned is a protocol fault, not an `assume_init_read` | planned |
| `MOLT_GRANT_OK` | an extent becomes reachable in a second view only through a grant | planned |
| `MOLT_REVOKE_OK` | after revoke, the second view faults — before the address is reissued to anyone | planned |

`MOLT_DOMAIN_ABSENT_OK` and `MOLT_RING_FAULT_OK` are new here; the rest already
appear in [`docs/address-space.md`](address-space.md)'s staging table. Both are
new because writing this document found the gaps they cover, which is the
argument for having written it before the code rather than after.

## The constraints this leaves on future code

A threat model that does not constrain anything is prose. These are the review
questions for every commit in Stage 5.0 and after:

- Does the domain's root table contain any kernel mapping, including the
  physmap? It must not, and `Audit` can answer it.
- Is `SUM`/SMAP clear outside an explicit, bounded window?
- Is the order unmap → shootdown → `retire` unbroken? No path may hand out an
  address whose epoch has not been swept.
- Does anything cross-domain use `SpscRing`, or `assume_init_read` on memory a
  domain can write? Both are the same bug.
- Does every submitted address get exactly one range check, on a kernel-local
  copy, against the extent of the capability that submitted it — with the
  out-of-range case masked rather than branched?
- Is any tag reused without a generation bump?
- Does any new protection depend on an attacker not knowing an address?

## What is not done yet

- **None of it is code.** Domains do not exist; this is the model the code will
  be reviewed against, and two of its markers are the review becoming testable.
- **No fuzzing of the ring validator.** The six rules deserve a host fuzzer over
  hostile index sequences before `MOLT_RING_FAULT_OK` means much; a single
  scripted attack proves the path, not the parser.
- **No quotas, no metering, no preemption**, as above.
- **Nothing about the IOMMU's own state.** The endpoint isolation shipped;
  whether a domain can influence *which* IOMMU domain a device lands in is a
  question for whenever a domain gets to own a device.
- **x86_64 tags are probed but not enabled.** `CR4.PCIDE` stays off until
  domains exist, so the x86_64 port flushes on every switch by design and the
  tagged path is exercised only on RISC-V today.

## The decision, restated

- **Isolation is absence, not permission** — the kernel and every other domain
  have no PTE in a domain's view, which is what makes the Meltdown family
  structurally inapplicable.
- **Addresses are not secrets.** Nothing may protect an extent by being hard to
  find, because a single address space has no re-roll.
- **Tags identify, they never authorise**, and a stale one is caught by
  generation before it is caught by hardware.
- **The cross-domain ring is a different type** from `SpscRing`, with a private
  consumer index, a validated producer index, a parsed POD payload, read-once
  fields, one masked range check, and rings inside the domain's own extent.
- **Spectre-v1 is answered by masking in the validator**, because it needs no
  barrier and both ports can have it; v2 and MDS are named as unaddressed rather
  than mitigated on paper.
- **Availability is not promised**, and the four ways it can be spent are
  enumerated instead of hedged.
