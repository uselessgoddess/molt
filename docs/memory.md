# Memory ownership

Status: Stage 2.0 decision record, July 2026.

Stage 2 was written as "first useful asynchronous I/O": PCI, MSI-X, a VirtIO
block driver, a filesystem. This document argues that the first item of Stage 2
is none of those, and that it is a memory model — then picks one.

## Why the order changes

Stage 1 treats physical memory as `u64`. `FrameAllocator` hands out a frame
address, the boot page table consumes it, and nothing records that this
happened. That is survivable while the only consumer is the boot page table,
because there is exactly one consumer and it runs once, single-threaded, before
interrupts.

A driver breaks all three properties at once. A VirtIO queue needs frames the
CPU maps write-back and the device reads by physical address; an MMIO window
needs frames the CPU must *not* cache and must never execute; a queue reset
needs to know that the frames it is about to reuse are not still being written
by a device that has not been told to stop. Each of those is a question about
*who owns which physical memory and what a mapping of it is allowed to do* —
and none of them can be answered by a `u64`.

So the issue's instinct is right: memory ownership comes first. With one
qualification that is worth stating, because it is the failure mode of doing
memory work up front — the model must be *sized by the first device*, not by
the last one. Stage 2.0 therefore builds exactly what PCI + VirtIO will need on
the next stage (typed spans, an owner per frame, MMIO windows that cannot be
cached or executed) and deliberately stops short of untyped retyping, a
capability derivation tree, and IOMMU domains, which have no consumer yet.

## What the three references actually do

The three systems named in the issue answer the *same* question — who owns a
frame — in three different places: in the type system, in a kernel object
graph, and in a per-frame array.

**Theseus keeps no per-frame metadata at all.** Ownership is a Rust type:
`Frames<const S: MemoryState, P: PageSize>`, where `MemoryState` is `Free`,
`Allocated`, `Mapped`, or `Unmapped`, with aliases `FreeFrames`,
`AllocatedFrames`, `MappedFrames`, `UnmappedFrames` ([frame_allocator][fa]).
The type is not `Clone`, so exclusive ownership is a compile-time property, and
`Drop` is state-dependent — dropping `AllocatedFrames` returns them to the
allocator, while dropping `MappedFrames` is a `panic!("We should never drop a
mapped frame! It should be forgotten instead.")`. Unmapping produces an
`UnmappedFrameRange`, which the book describes as "a trusted 'token' stating
that the included frames cannot possibly still be mapped by any pages"
([book][tbook]). The allocator itself tracks free *chunks*, not frames, so its
cost is O(number of chunks). Note that the current code is not the OSDI'20
paper: today's `MappedPages` holds `{page_table_p4, pages, flags}` and no
frames ([mapper.rs][tmap]); the paper's `MappedPages { pages, frames, flags }`
is the 2020 design.

**seL4 keeps no per-frame metadata either**, and for a sharper reason: memory
is user-supplied. `seL4_Untyped_Retype()` splits an untyped region into typed
objects, and derivations are recorded in the capability derivation tree, which
"is implemented as part of the CNode object and so requires no additional
kernel meta-data" (manual §2.4.1, fn. 3). Reuse goes through
`seL4_CNode_Revoke()` on the untyped cap, after which "no references remain to
any object within the untyped region, and the region may be safely retyped
again" (§2.4.1). Two details are worth copying even without capabilities.
First, untyped memory is tagged *device or general purpose* at the root, the
tag is inherited by children and cannot be changed, and device frames "cannot
be set as thread IPC buffers, or used in the creation of an ASID pool" (§2.4) —
device-ness is a property of the memory, not of the request. Second,
cacheability is a *separate argument from rights*: `seL4_X86_Page_Map(...,
seL4_CapRights_t rights, seL4_X86_VMAttributes attr)` (§10.4.11.4), with
attributes `WriteBack`, `CacheDisabled`, `WriteThrough`, `WriteCombining`
(§7.2). Rights are Read/Write/Grant/GrantReply and are downgrade-only.

**Redox does keep per-frame metadata**, because it has untrusted userspace and
mutable mappings to protect: a `PageInfo` per allocator-returnable frame, two
words wide — `refcount: AtomicUsize` and `next: AtomicUsize` ([memory/mod.rs][rmem])
— stored in a Linux-like `SECTIONS` array. It was introduced precisely because
the kernel "previously didn't store any metadata about physical memory frames,
allowing malicious schemes to continue using munmapped pages" ([kernel-9][rk9]).
The two words double as the p2buddy freelist when a frame is free
([kernel-10][rk10]), and Redox itself calls out the cost: storing "511 or in
the extreme case 262,143 useless PageInfos" once large pages arrive "is of
course not efficient". The pragmatic bit Molt copies is a policy, not a data
structure: `Provider::PhysBorrowed` is documented as "the kernel will forbid
borrowing any physical memory range, that the memory map has indicated is
regular allocatable RAM" ([context/memory.rs][rctx]). That is the whole
"driver mapped an arbitrary physical address" class of bug, closed by
classifying memory rather than by trusting the caller.

## What Molt takes

Molt has one compiler-checked address space and no process abstraction, so it
sits closer to Theseus than to either of the others. Three ideas, one from
each:

- **from Theseus** — ownership is a value, not a table lookup. `Frames` is not
  `Copy`, has no constructor, and is consumed by release.
- **from seL4** — what memory *is* is decided at the root and inherited, not
  requested; and cacheability is orthogonal to rights.
- **from Redox** — one small per-frame word, and a hard rule that RAM in the
  firmware map can never be borrowed as a device window.

And three deliberate omissions: no CSpace or capability derivation tree
(`molt-core` already has generation-checked `CapabilityTable`, and a second
handle space would be a duplicate); no untyped/retype (there is no untrusted
caller to protect against yet, and retyping without one is ceremony); no
per-frame refcount (there is no `mmap`, no copy-on-write, and no sharing
between address spaces, so the count would be 0 or 1).

## The model

`molt-arch::memory` separates three things that are easy to collapse and
painful to separate later:

| question | type | source of truth |
| --- | --- | --- |
| what is this memory? | `Kind` | the firmware memory map |
| who holds it now? | `Owner` in a `FrameTable` | the kernel |
| what may a mapping grant? | `Rights` + `Cache` | checked against `Kind` |

```rust
let inventory = Inventory::new(boot_info.memory_map());
let window = inventory.device(Span::new(0xfe00_0000, 0xfe01_0000)?)?;
let (rights, cache) = window.mapping(Rights::READ_WRITE)?;   // always Cache::Device

let mut frames = FrameTable::over(pool, &mut slots)?;
let queue = frames.claim(Span::frames(pool.start(), 4)?, Owner::Device(0))?;
frames.release(queue)?;
```

- `Span` is a frame-aligned, non-empty, non-inverted physical range. Every
  other type takes one, so alignment is checked once.
- `Kind` is `Ram | Image | Reserved | Device`, read from the boot map by
  `Inventory`. A device window exists only where firmware left a *hole*, which
  is Redox's rule expressed as a type: `Inventory::device` on RAM is
  `Error::Kind`, so "map me this physical address" is not a function that
  exists.
- `Kind::allows` is where policy lives: `Device` may not be executable
  (`MappingError::Permissions`) and may not be write-back
  (`MappingError::Cacheability`); RAM may not be mapped with device ordering;
  `Reserved` may not be mapped at all.
- `Rights` is read/write/execute with W^X rejected at construction, and a
  read bit made explicit because DMA needs "readable, not writable" and
  Stage 1's `MapPermissions` could not say that.
- `Cache` has two values. Write-combining framebuffers are the obvious third
  and have no consumer, and an unused variant is an untested one.
- `Frames` is the ownership token. It has no `Drop`: releasing needs the table,
  a `Drop` cannot reach one without a global, and a global frame table is what
  this stage is trying not to need yet. `#[must_use]` catches the case that
  matters. This is weaker than Theseus's state machine, and it is the one place
  Molt knowingly takes the lesser design — see the limits below.
- `FrameTable` stores one `Option<Owner>` per frame in a *caller-supplied*
  slice, so `molt-arch` still allocates nothing and the kernel decides whether
  it is tracking all of RAM or the eight frames a driver may be handed. At one
  byte per frame that is 0.024% of RAM, against Redox's 16 bytes and 0.39%.

## Where it is enforced

Policy that is only checked on the way in is a convention. The existing audit
(`docs/testing.md`, `Audit::cover` / `Audit::accepts`) walks the *live* page
tables, and it now understands device memory: `Contents::Device` rejects a leaf
that is executable or write-back. That check fails closed — `PageProtection`
defaults to `Cache::WriteBack`, so a platform that does not report the memory
type of its leaves fails an MMIO audit rather than silently passing one. That
is the direction an unaudited attribute should fail, and it is a standing task
for whichever platform reports first.

Stage 2.2 put that direction to work: configuration space, a device's BAR, and
an MSI-X table are all reached through the same `Inventory::device` window and
the same audit, so a bus is not a special case of a device window — see
[the bus decision record](pci.md).

Two boot markers cover the rest. `MOLT_PHYSMAP_OK` reads firmware's map back
as typed memory — usable RAM classifies as `Kind::Ram`, the space above every
reported region is a hole and therefore `Kind::Device`, and asking for a device
window inside RAM fails. `MOLT_FRAME_OWNER_OK` claims frames, fails to claim
them twice, and gets them back on release. Both run on x86_64 and RISC-V.

## Limits

- **`Frames` can be leaked.** `#[must_use]` is a lint, not a proof; dropping
  the token loses the frames until reboot. Theseus's `Drop` is stronger and
  needs a global allocator to reach, which Stage 2.0 does not have. Nothing
  becomes *unsound* — the frames stay marked as owned, which fails closed.
- **Ownership is advisory across the address space.** Every cell is trusted
  and shares one address space, so `Owner` prevents a double *allocation*, not
  a stray write. That is the same trade the security model already states, and
  it is why revocation only becomes load-bearing when there is an IOMMU or an
  untrusted cell.
- **DMA is not isolated.** A device given a physical address can write
  anywhere, `Owner::Device` or not. Until Stage 4's IOMMU work, the honest
  statement is that DMA buffers are *tracked*, not *contained*.
- **x86_64 pins the loader's boot windows.** The kernel builds its own tables
  and switches `CR3` onto them, so both `cover` and `accepts` run on either
  platform. The image, the stack, and the boot info keep the addresses the
  loader chose, because `CR3` is written from code running at those addresses;
  the stack and the boot info are pinned through `BootloaderConfig` so the
  kernel can find them again, and the image window follows the loader's mapping
  past the reported file length because `.bss` lives beyond it.
- **Cacheability is applied, one memory type per platform.** x86_64 pins PAT to
  its reset configuration and selects the uncacheable entry with `PCD|PWT` for
  device windows, so a `Cache::Device` leaf really is uncacheable rather than
  merely declared so. RISC-V's Sv39 PTE has no memory-type field at all:
  `Svpbmt` is absent on QEMU `virt`, so the effective type is the PMA of the
  physical address and the audit reports it from `Inventory::kind`. Stage 2.2
  added a device-tree reader (`docs/pci.md`), so detecting `Svpbmt` is now a
  question of reading one more node rather than of having no reader at all — it
  becomes an override on top of the PMA, not a replacement for it.
- **Write-combining still has no consumer.** `Cache` remains two values; a
  framebuffer that wants a third can add it with a test that fails without it.

## References

- [Theseus frame_allocator][fa]
- [Theseus mapper.rs][tmap]
- [Theseus book: mapping virtual to physical memory][tbook]
- [Theseus OSDI 2020 paper](https://www.usenix.org/system/files/osdi20-boos.pdf)
- [seL4 reference manual](https://sel4.systems/Info/Docs/seL4-manual-latest.pdf)
- [Redox kernel: memory/mod.rs][rmem]
- [Redox kernel: context/memory.rs][rctx]
- [Redox: on-demand paging II][rk9]
- [Redox: kernel performance and correctness][rk10]

[fa]: https://github.com/theseus-os/Theseus/blob/theseus_main/kernel/frame_allocator/src/lib.rs
[tmap]: https://github.com/theseus-os/Theseus/blob/theseus_main/kernel/memory/src/paging/mapper.rs
[tbook]: https://www.theseus-os.com/Theseus/book/subsystems/memory_mapping.html
[rmem]: https://gitlab.redox-os.org/redox-os/kernel/-/blob/master/src/memory/mod.rs
[rctx]: https://gitlab.redox-os.org/redox-os/kernel/-/blob/master/src/context/memory.rs
[rk9]: https://www.redox-os.org/news/kernel-9/
[rk10]: https://www.redox-os.org/news/kernel-10/
