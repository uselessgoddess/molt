# VirtIO devices

Status: Stage 4.6 isolated block/network implementation record, August 2026.

Why a queue is built out of frames the kernel owns, how mappings turn those
frames into device-scoped IOVAs, what the request semantics promise, and how
writes and flushes are ordered. The stable block and isolation boundary is
specified in [`block.md`](block.md); this document records the VirtIO transport.

## The shape of the problem

A VirtIO device is reached three ways at once, and Stage 2.2 already drew the
line each of them sits on:

- **The transport structures** — common configuration, notification, and the
  device-specific block — live in a BAR. That is a device-supplied address, so
  it is classified through `Inventory::device` and mapped as an `Mmio` window
  before anything touches it, exactly as [`docs/pci.md`](pci.md) describes.
- **The virtqueue** is shared memory the *driver* owns and the device reads and
  writes through a mapped IOVA (or the typed identity backend). The device does DMA into
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

The arena tracks its span a frame at a time, in the frame table it already
keeps. A block read needs five regions — three ring structures, a
request-header block, and a data buffer — and each claims the lowest free run
long enough to hold it. [`release`](../crates/molt-arch/src/dma.rs) hands one
region's frames back for the next region to use, so a driver that reprograms a
queue reuses its span instead of running it down;
[`reset`](../crates/molt-arch/src/dma.rs) evicts the tag wholesale, whatever is
still outstanding. Both are for a device already told to stop, which is the
point the four semantics below are built around.

## Where a device address comes from

The CPU reaches a [`Region`](../crates/molt-arch/src/dma.rs) through its private
direct-map pointer, while a [`Mapper`](../crates/molt-arch/src/iommu.rs) turns
the region's physical backing into a device-scoped [`Mapping`](../crates/molt-arch/src/iommu.rs).
The identity backend's IOVA equals the physical base; the VirtIO-IOMMU backend
allocates a translated IOVA. The queue does not know or care which was chosen.

`Mapping` owns the region for as long as a device can reach it and carries the
requester ID and DMA permissions. `readable` and `writable` check both bounds
and permission and return a `DmaSlice`; only that checked slice can become a
VirtIO descriptor. A block read builds three slices: header readable, data
writable, and status writable. No public queue operation accepts a bare
physical address or IOVA.

Unmap consumes the mapping and is the only safe path back to the region. An
unmap failure returns the still-live mapping rather than memory the caller
could recycle. [`block.md`](block.md) gives the complete backend and teardown
contract.

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

**Timeout is a line budget, not a promise the device answers.** The driver polls
the used ring, waits through `Arrivals` when work remains, then polls once more
if that wait expires so a lost or coalesced interrupt cannot hide an already
completed request. Only an empty final poll becomes `BlockError::Timeout`.

**Cancellation gives up on a request without lying about its descriptors.**
This is the subtle one. When `read` times out it calls
[`Requests::cancel`](../crates/molt-virtio/src/request.rs) — but it does *not*
free the descriptor head. The device may still be about to write that buffer;
handing the head back to the free list would let the next request reuse a
descriptor the device is mid-DMA into. So the head and that request's bounce
slot stay reserved, the token is marked cancelled, and when the device returns
it the completion is recognized as `Completion::Stale` and dropped rather than
delivered to a caller that walked away. This is the same generation-stamped discipline
`CompletionSlab` and `InterruptSlab` use — a `Token` carries the slot's
generation, the generation bumps on every completion, and an old token can no
longer match a slot that has been reused. Cancellation and stale-rejection are
one mechanism seen from two ends.

**Queue reset reclaims frames only after the device is told to stop.** This is
the fourth acceptance box and the ordering is the whole point.
`Block::reset` resets the device *first* — writing zero to the status register
and waiting for the device to clear it. It then consumes each mapping through
the selected backend and returns the resulting regions to the arena. Only an
empty domain is detached. Reverse that order and a frame could rejoin the free
pool while an in-flight descriptor still points at it. `reset` takes `self`, so
a driver cannot submit through a block queue whose mappings it returned.

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
zero. The stable `BlockOp` contract exposes both without exposing a virtqueue.
Its driver runs flush alone; reads and writes can complete out of order. MoltFS
places explicit flushes between log data and the superblock that names it.

**Bus mastering follows isolation.** On the x86_64 smoke the kernel keeps each
block or network function's `BUS_MASTER` bit clear, attaches its requester to a
distinct VirtIO-IOMMU domain, installs all queue and buffer mappings, and
negotiates `ACCESS_PLATFORM`. Only then may the function initiate transactions.
It is disabled again after device reset and before the empty domain is
detached. The identity backend remains available for platforms without an
IOMMU and makes no claim that bus mastering is isolated.

**Block completion is interrupt-driven with a final polling fallback.** Queue
zero is routed through MSI-X. The driver polls before waiting and once after an
expired wait; an interrupt is therefore the normal wake without being a single
point of failure for a completion already visible in the used ring.

## The interrupt-driven network pair

`molt_virtio::Net` reuses the same transport, split queue, and DMA arena but
programs receive queue 0 and transmit queue 1. Both queue configuration records
name one MSI-X table entry, which the kernel binds to `InterruptSlab` before it
enables the device. The PCI command initially grants memory decode and disables
INTx while leaving bus mastering clear. Startup receives a `NetConfig` plus its
mapper, requires `ACCESS_PLATFORM` for a translated mapper, and returns that
mapper only after reset. Bus mastering is granted only after both queues and
all packet buffers are installed.

The driver requires `VIRTIO_F_VERSION_1`, `VIRTIO_NET_F_MAC`, and
`VIRTIO_NET_F_MRG_RXBUF`. The merged-buffer feature makes the modern 12-byte
header, including `num_buffers`, explicit across QEMU versions. Molt does not
accept checksum or segmentation offloads; each receive buffer is therefore the
full 1526 bytes and must carry one complete maximum-size Ethernet frame with
`num_buffers == 1`.

Receive ownership is a cycle, not an allocation. Startup posts one writable
buffer for every queue slot before setting `DRIVER_OK`. A completion maps its
descriptor head back to that buffer, validates the device header, copies the
bounded Ethernet frame to the protocol layer, and republishes the same buffer
before returning — including on malformed input. Thus a bad packet cannot
silently drain the queue. Transmit keeps one frame in flight and reports
backpressure until the used ring returns it.

Reset preserves the same ordering as block: device status reaches zero before
either queue or any packet buffer returns to the arena. Only after reset does
the kernel mask and disable MSI-X and release the interrupt token.

## How it is tested

Every piece with arithmetic or a state machine has host coverage under Miri: the
transport capability walk and its refusals, the queue's chain-and-publish and
free-list reclaim, the request table's deliver/cancel/stale transitions, the
handshake's status accumulation and ring programming, and the arena's
contiguity, disjointness, and reset. The fences the split virtqueue depends on
are the same `Release`/`Acquire` pair `molt-core` already exercises under loom.

What no host test can show is that claimed frames, installed IOVAs, a mapped
BAR, and QEMU's translator all describe the same disk. The x86_64 smoke adds
`virtio-iommu-pci` and a `virtio-blk-pci` with `iommu_platform=on`, backed by a
MoltFS image `xtask mkfs` lays out from `disk/`. It requires markers for attach,
five installed mappings, two simultaneous reads, block IRQ completion, the
existing filesystem write/restart path, a clean fault event queue, and ordered
reset/unmap/detach. The full list and its meaning are in [`block.md`](block.md).

The same smoke then brings up modern VirtIO-net over a second twelve-frame arena
and isolated domain. `MOLT_NET_IOMMU_OK` requires mapping before bus mastering;
`MOLT_NET_OK` requires its two queues and MSI-X route to complete startup.
`MOLT_UDP_OK` requires an ARP exchange followed by a DNS response through
Ethernet, IPv4, the nested IP ring, and the capability-addressed UDP ring. Host
tests separately damage every wire header and return one RX descriptor through
the used ring to prove it is immediately reposted.

The disk is a real filesystem rather than a signed pattern so that one artifact
carries the whole path: the same bytes this driver reads are what MoltFS writes,
mounts, and prints through its shell, and the markers between `MOLT_BLOCK_OK`
and `MOLT_VIRTIO_RESET_OK` are that filesystem's. See [`docs/fs.md`](fs.md).

The RISC-V smoke does not run it. The `virt` board hands out no DMA frames to
this path — `free_frames` is `None` — so the driver reports
`MOLT_VIRTIO_SKIPPED` and the enumeration carries on, the same honesty the RISC-V
interrupt path already practices.
