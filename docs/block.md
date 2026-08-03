# Block I/O and DMA isolation

Status: Stage 4.6 implementation record, August 2026.

This is the stable boundary shared by VirtIO block and NVMe. It records what
owns a request, which address a descriptor or PRP may contain,
and the order that makes unmap and frame reuse safe. The VirtIO transport
details remain in [`virtio.md`](virtio.md).

## The block contract stays above the transport

`molt-block::BlockOp` is the operation passed through the storage ring:

- `Read` and `Write` carry their owned 4 KiB buffer, a sector, and the number
  of valid bytes.
- `Flush` is an ordering barrier. `BlockDriver` starts it only after all older
  requests complete and starts nothing newer until the flush completes.
- `RequestId` is stable from submission to `BlockDone`, including when a
  device completes requests out of order.

A hardware driver implements `molt_block::Queue`: `start` takes ownership,
`reap` returns the same operation's buffer and result, and `depth` states the
real number of requests the device can own. Both hardware adapters expose eight
independent request slots. VirtIO matches a used-ring head to its header,
status, bounce window, and generation-stamped token. NVMe uses the slot as its
command identifier and matches it from the completion queue. Several commands
can therefore be published before an interrupt, and neither adapter assumes
completion order.

Device status other than `VIRTIO_BLK_S_OK` becomes `BlockError::Device`.
Malformed completions are rejected. `Arrivals` is the normal completion path;
after an interrupt the driver drains every used entry it can see. If the
platform wait budget expires, one live operation completes with
`BlockError::Timeout`, but its descriptors and DMA slot remain reserved until
the late completion is recognized as stale. A timeout never turns potentially
live DMA memory into somebody else's allocation.

## DMA addresses are capabilities, not integers

`molt-arch::iommu` separates four values that used to collapse into a `u64`:

- `Region` owns page-backed CPU/physical memory.
- `DeviceId` names the PCI requester whose transactions may use a mapping.
- `Iova` is an address in that requester's device-visible address space.
- `Mapping` owns the `Region` while the mapping is installed and records its
  `DeviceId`, `Iova`, and `DmaPerm` (`READ`, `WRITE`, or `READ_WRITE`).

Only `Mapping::readable` and `Mapping::writable` mint a `DmaSlice`, and a
virtqueue descriptor accepts only that type. The slice checks bounds and
permission before exposing an IOVA. There is no public descriptor constructor
from a physical address. `Mapper::unmap` consumes the `Mapping`; on success it
returns the `Region`, while failure returns an `UnmapError` that still owns the
possibly reachable mapping. These consuming transitions make safe double
unmap and frame reuse while mapped unrepresentable.

The queue constructor also checks its structural mappings: descriptors and the
available ring are device-readable, while the used ring is both readable and
writable because the device maintains its index as it appends completions.

An arena reports whole pages even when a queue structure uses only a few
bytes. That is the physical extent an IOMMU can map without exposing bytes
owned by another allocation. Descriptors remain bounded to the smaller
structure through `DmaSlice`.

## Mapper backends

`Identity` preserves the typed lifecycle on machines where device addresses
are physical addresses. It does not claim isolation and does not request
`VIRTIO_F_ACCESS_PLATFORM`.

`Fake<N>` is a host-test backend. Its fixed IOVA allocator rejects overlaps,
reuses only released ranges, scopes translation by `DeviceId`, and enforces
read/write permissions. It exercises the same consuming `map`/`unmap` API
without hardware.

`molt_virtio::Iommu` drives VirtIO device ID 23. Its own request and event
queues are identity-mapped, polling control-plane memory; the QEMU PCI function
does not require MSI-X. For each attached endpoint it:

1. negotiates input range, domain range, and map/unmap support;
2. allocates a distinct non-bypass domain and attaches the PCI requester while
   PCI bus mastering is still disabled;
3. allocates page-aligned IOVAs, preferring an aperture above 4 GiB so an
   accidental physical-address descriptor is visible in QEMU;
4. sends synchronous MAP/UNMAP commands with inclusive ends and exact device
   permissions; and
5. keeps every event-queue buffer posted, parses asynchronous faults, and
   exposes the latest fault for platform diagnostics.

Mapped drivers must negotiate `VIRTIO_F_ACCESS_PLATFORM`; startup fails if the
device does not offer it. The x86_64 smoke boots QEMU `q35` with
`virtio-iommu-pci` and a block endpoint configured with `iommu_platform=on`.
Only after attach and all mappings complete does the kernel set an endpoint's
PCI `BUS_MASTER` bit. The same path isolates VirtIO block, VirtIO network, and
NVMe requester IDs without sharing translations. Detach releases only that
endpoint's empty domain. Shutdown reverses the dependency: reset the endpoint,
unmap every region, disable bus mastering, detach the empty domain, then reset
the IOMMU control device.

Run the complete path with:

```console
cargo xtask smoke x86_64
```

The generated command uses `cache=none` for the raw MoltFS disk, so the guest's
flush request remains the durability boundary before a superblock publish.

MSI-X writes are a platform-reserved MSI range and bypass translation as the
VirtIO-IOMMU reserved-memory model requires. Ordinary descriptor, ring,
control, and data accesses must resolve through the endpoint's domain. Other
QEMU devices remain on the IOMMU's boot-bypass path until a driver explicitly
attaches them; that is compatibility, not isolation for those devices.

## Verification and measurement

Host tests cover IOVA overlap/reuse, double release, device and domain scoping,
permissions, request encodings, event parsing, cyclic descriptor rejection,
two requests published together, reordered reads, and device error status.
The x86_64 smoke requires each endpoint to attach before bus mastering, two
simultaneous reads on both storage transports, interrupt completion, clean
fault queues, ordered teardown, and the existing MoltFS write/restart markers.

`cargo xtask bench` retains the four filesystem measurements and adds
`block_queue/depth/{1,8}`. This host benchmark measures the stable queue and
buffer path; it does not pretend to be a QEMU or hardware latency number.

## NVMe queue

`molt-nvme` drives namespace 1 through one admin pair and one I/O pair. It
validates the NVM command set and 4 KiB page support, identifies controller and
namespace, accepts metadata-free 512-byte through 4 KiB LBA formats, negotiates
one queue pair, and creates a nine-entry hardware queue. The ninth entry keeps
the ring's required empty slot while eight `BlockOp`s are live. Each command
has one private 4 KiB PRP-backed page; reads, writes, and flushes retain the
same sector and durability contract as VirtIO.

Preparation and enablement are separate types. `Prepared::prepare` disables
the controller, maps six regions for the NVMe requester, and programs the admin
queue addresses while PCI bus mastering is clear. Only after the kernel grants
that bit can `Prepared::enable` set `CC.EN` and issue Identify and queue-creation
commands. Reset clears `CC.EN`, waits for `CSTS.RDY` to clear, consumes every
unmap, and only then returns the frames. The QEMU smoke uses a second raw MoltFS
image and requires Identify, two live reads, write/flush/readback, a clean IOMMU
event queue, and ordered reset. VirtIO and NVMe both expose depth eight; this is
a bounded-driver comparison, not a throughput claim.
