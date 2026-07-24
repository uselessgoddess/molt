# VirtIO block

Status: Stage 2.3 decision record, July 2026.

Why a queue is built out of frames the kernel owns rather than memory the device
names, where a physical address is allowed to exist, what the four request
semantics actually promise, and how Stage 3 orders writes and flushes. Written
as the record for `molt-arch::dma` and the `molt-virtio` crate.

## The shape of the problem

A VirtIO device is reached three ways at once, and Stage 2.2 already drew the
line each of them sits on:

- **The transport structures** — common configuration, notification, and the
  device-specific block — live in a BAR. That is a device-supplied address, so
  it is classified through `Inventory::device` and mapped as an `Mmio` window
  before anything touches it, exactly as [`docs/pci.md`](pci.md) describes.
- **The virtqueue** is shared memory the *driver* owns and the device reads and
  writes by physical address. This is the new thing: the device does DMA into
  it, so the frames behind it have to be frames the kernel can account for and
  reclaim, not a buffer the device was pointed at and trusted to respect.
- **The notification** is a store into a third BAR region that tells the device
  fresh descriptors are ready.

The first and third are windows, and the borrow on the window is their
authority. The second is where DMA enters the model, and it is what this stage
is really about.

## The queue is frames the kernel owns

The queue is not allocated. It is *claimed*:
[`Arena::claim`](../crates/molt-arch/src/dma.rs) takes a contiguous span of
frames from the same [`FrameAllocator`](memory.md) the kernel draws its own
tables from, stamps them `Owner::Device(tag)` in a `FrameTable`, and hands
regions out of that one span. So every byte the device can DMA into is a frame
the frame table knows is spoken for, by whom, and for how long — the same
ownership discipline the kernel already applies to page-table frames, extended
to the one other thing on the machine that writes memory without going through
the CPU.

The tag matters. `Owner::Device(u32)` is the opaque handle
[`docs/pci.md`](pci.md) introduced for a line the fabric owns; here it names the
driver a span belongs to. A frame in a device arena is not `Owner::Kernel` and
not free — a later audit walking the table sees device-owned memory as exactly
that, and a second claim over the same span is `Error::Owned` rather than a
silent overlap of two devices' DMA.

The arena is bump-allocated and reclaimed *whole*. A block read needs five
regions — three ring structures, a request-header block, and a data buffer —
and rather than a free list with per-region lifetimes, the arena hands them out
in rising order and takes the entire span back at once in
[`reset`](../crates/molt-arch/src/dma.rs). A driver that owns one queue has one
thing to release, and it releases it at one point in its life, which is the
point the four semantics below are built around.

## Where a physical address becomes something you may touch

The device speaks physical addresses; the CPU speaks pointers. A
[`Region`](../crates/molt-arch/src/dma.rs) is the pair, and it is the reason no
public operation in `molt-virtio` passes a raw physical address around.

A region carries the physical base the device is given
([`physical`](../crates/molt-arch/src/dma.rs)) and, privately, the write-back
direct-map pointer the driver reaches the same bytes through. Every accessor is
bounds- and alignment-checked against the region's declared length, so a
descriptor that would point the device past its buffer is a `DmaError::Range`
the driver never emits, not a stray DMA. The `cpu = offset + physical`
relationship — `offset` being the platform's direct-map base — is established
once, inside `Arena::region`, under the one `unsafe` block that can see both
halves; the rest of the driver only ever holds the safe handle.

`Segment` is the other half of the discipline. A queue descriptor is built from
`Segment::readable(physical, len)` or `Segment::writable(physical, len)` — the
physical address comes straight off a `Region`, and readable-versus-writable is
which way the *device* may touch it. The block read builds exactly three: the
header readable, the data buffer writable, the status byte writable. There is no
constructor that takes a bare address, so a segment always names a range some
region already vouched for.

`Region`, like `Mmio`, is `Send` but not `Sync`. A DMA buffer is
order-sensitive shared state; two cores writing one interleaved is a driver bug
that reads as a device fault, and the type refuses to let it compile rather than
letting it happen at three in the morning.

## The four semantics

Stage 2.3's acceptance names cancellation, timeout, queue reset, and
backpressure. They are not four features; they are four answers to the one
question a shared ring keeps asking — *who is allowed to touch this descriptor
now* — and they are worth stating as promises.

**Backpressure is the queue refusing, not the queue growing.** The free
descriptor list is a fixed stack sized at [`MAX_SIZE`](../crates/molt-virtio/src/queue.rs);
`Queue::push` reserves a whole chain before writing any of it and returns
`VirtioError::Full` when the chain will not fit. There is no heap to grow into
and no blocking — `Full` is the signal a caller drains completions against
before submitting again. A ring that silently overwrote an in-flight descriptor
would be handing the device two meanings for one slot, which is the corruption
this replaces with an error.

**Timeout is a bounded spin, not a promise the device answers.** `Block::read`
polls the used ring up to `TIMEOUT_SPINS` times and then gives up with
`VirtioError::Timeout`. The number is a spin budget, not wall-clock — there is
no timer on this path — but the property that matters is that a wedged or absent
device cannot hang the caller forever. Polling at all, rather than waiting on an
interrupt, is deliberate for this stage: no MSI-X vector is routed to the block
device, so the driver drains the used ring itself. The interrupt-driven path is
Stage 3's, when there is a scheduler with something better to do than spin.

**Cancellation gives up on a request without lying about its descriptors.**
This is the subtle one. When `read` times out it calls
[`Requests::cancel`](../crates/molt-virtio/src/request.rs) — but it does *not*
free the descriptor head. The device may still be about to write that buffer;
handing the head back to the free list would let the next request reuse a
descriptor the device is mid-DMA into. So the head stays reserved, its slot
marked `Cancelled`, and when the device finally returns it the completion is
recognized as `Completion::Stale` and dropped rather than delivered to a caller
that walked away. This is the same generation-stamped discipline
`CompletionSlab` and `InterruptSlab` use — a `Token` carries the slot's
generation, the generation bumps on every completion, and an old token can no
longer match a slot that has been reused. Cancellation and stale-rejection are
one mechanism seen from two ends.

**Queue reset reclaims frames only after the device is told to stop.** This is
the fourth acceptance box and the ordering is the whole point.
`Block::reset` resets the device *first* — writing zero to the status register
and waiting for the device to clear it, so the device has provably stopped
reading the rings — and only then calls `Arena::reset` to return the frames to
the table. Reverse that order and a frame could rejoin the free pool while an
in-flight descriptor still points the device at it, so the next owner of that
frame inherits a stray DMA write. The type system helps here too: `reset` takes
`self` by value, so a driver cannot read a sector through a `Block` whose frames
it has already handed back.

## Bringing the device up

The handshake in [`config.rs`](../crates/molt-virtio/src/config.rs) is the
modern one and has exactly one point of policy: `negotiate` always demands
`VIRTIO_F_VERSION_1` and refuses a device that will not offer it. There is no
legacy fallback. A device that clears `FEATURES_OK` after the driver writes it,
or that offers no modern transport, is refused rather than driven through an
interface with a different memory model. The block driver also refuses
`VIRTIO_BLK_F_RO` and requires `VIRTIO_BLK_F_FLUSH`. A device without a
durability boundary cannot satisfy the filesystem checkpoint contract and is
refused during startup.

`clamp_queue` caps the device's advertised queue depth at what the driver can
host without a heap and refuses a device that offers no queue at all. The device
picking a smaller queue than it advertised, or a non-power-of-two size, is a
`VirtioError::Device` rather than a ring laid out wrong.

## Write ordering

`VIRTIO_BLK_T_OUT` uses a device-readable data descriptor.
`VIRTIO_BLK_T_FLUSH` carries only request and status descriptors, with sector
zero. `molt-block::Writable` exposes both without exposing a virtqueue. One
outstanding request at a time keeps completion order deterministic, and MoltFS
places explicit flushes between log data and the superblock that names it.

**Bus mastering is granted for this device, once, and it is not free.** The same
trade [`docs/pci.md`](pci.md) recorded for MSI applies with full force here: a
device with `Command::BUS_MASTER` set can DMA anywhere in physical memory, and
until there is an IOMMU that is a trust decision. Stage 2.2 granted it so an MSI
could be posted; Stage 2.3 is where it is granted so the device can read the
virtqueue at all, which is the more honest version of the same cost. The kernel
sets `MEMORY | BUS_MASTER` on exactly the one function it chose, in `virtio.rs`,
after classifying that function's BAR — an interrupt-capable or DMA-capable
device on this kernel is as privileged as the kernel, and the arena's frame
ownership is what bounds *where* it writes in practice, not hardware isolation.

**Completion is polled, and only one queue is programmed.** No MSI-X vector is
routed to the block device and only queue zero is built. A block device with
multiple queues, or one driven from an interrupt, is Stage 3's concern, when a
scheduler exists to be woken.

## How it is tested

Every piece with arithmetic or a state machine has host coverage under Miri: the
transport capability walk and its refusals, the queue's chain-and-publish and
free-list reclaim, the request table's deliver/cancel/stale transitions, the
handshake's status accumulation and ring programming, and the arena's
contiguity, disjointness, and reset. The fences the split virtqueue depends on
are the same `Release`/`Acquire` pair `molt-core` already exercises under loom.

What no host test can show is that a queue built from claimed frames, a device
brought up over a mapped BAR, and a physical address handed across the DMA
boundary all describe the same disk. The only proof is a sector reading back
correct, so the x86_64 smoke attaches a `virtio-blk-pci,disable-legacy=on`
function backed by a MoltFS image `xtask mkfs` lays out from the `disk/` tree,
brings the device up, reads sector zero, commits a filesystem write, and resets.
It requires `MOLT_VIRTIO_OK`, `MOLT_BLOCK_OK`, `MOLT_FS_WRITE_OK`, and
`MOLT_VIRTIO_RESET_OK` on the serial line. The last marker proves queue-reset
ordering: the device stopped, then frames came back.

The disk is a real filesystem rather than a signed pattern so that one artifact
carries the whole path: the same bytes this driver reads are what MoltFS writes,
mounts, and prints through its shell, and the markers between `MOLT_BLOCK_OK`
and `MOLT_VIRTIO_RESET_OK` are that filesystem's. See [`docs/fs.md`](fs.md).

The RISC-V smoke does not run it. The `virt` board hands out no DMA frames to
this path — `free_frames` is `None` — so the driver reports
`MOLT_VIRTIO_SKIPPED` and the enumeration carries on, the same honesty the RISC-V
interrupt path already practices.
