# Networking

Status: Stage 3 design, July 2026. Analysis and plan, not yet code.

The next Stage 3 item is a network path: VirtIO-net, Ethernet, ARP, IPv4, then
UDP, then TCP. This is the decision record before the first packet — what the
stack borrows from seL4, Theseus, Redox, and smoltcp, what it deliberately does
not, and how it lands on the primitives the kernel already has: DMA arenas,
paired rings, typed capabilities, cells, and the registry.

## What the existing primitives already decide

A block device and a network device are the same problem seen twice: frames the
kernel owns, handed to hardware, completed through a ring. Stage 2.3 already
built that for `molt-virtio`:

- `molt_arch::dma::Region` — frames the device reads and writes, returned to an
  arena only after the device is told to stop.
- `Queue` — one split virtqueue over those regions, with `Segment`/`Used`.
- `Transport`/`Common`/`Notify` — the modern-VirtIO handshake, device-agnostic.

VirtIO-net uses the same split virtqueues as VirtIO-block: a receive queue the
device fills, a transmit queue the driver fills, each descriptor a `virtio_net_hdr`
followed by a frame. So the driver is `molt-virtio`'s existing transport with a
second device type, not a new stack. This is the first reason UDP is not
special: the wire is a ring the kernel owns, exactly like the disk.

The second is the shape above the driver. `molt-block` is a `Device` trait a
filesystem talks to without seeing a virtqueue. Networking wants the same seam:
a `Link` trait — frames in, frames out — that `molt-virtio-net` implements and a
loopback implements for host tests, so the protocol code is tested off hardware
the way MoltROFS is tested over a loopback disk.

## The kernel is not tied to a protocol

The issue's framing: there is no UDP abstraction in the kernel. There is a
`udp-cell` and a `tcp-cell`, each a service like `FsCell` — it mounts an
endpoint, answers on a ring, and restarts with its handles revoked. Inside a
cell lives the addressing (IPv4, later IPv6); the kernel below it moves frames
and routes interrupts and owns nothing about ports or sequence numbers.

This is the same move MoltFS made: the kernel has no `open(2)` and no path
resolver, only rings and capabilities, and the filesystem is a cell built out of
those. A socket is likewise not a kernel object. A cell publishes a `Net` scheme
in the registry; a client acquires a lease; a datagram is a ring operation
addressed by `Capability<Socket>`, with no ambient port table and no global
namespace — the same argument `docs/fs.md` makes about what *typed* buys, applied
to the network.

## What the surveyed systems settle

**seL4** keeps the network stack entirely out of the kernel: the microkernel
routes the NIC's interrupt and grants the driver its MMIO and DMA, and every
byte of Ethernet/IP/UDP lives in userspace components wired by capabilities.
Molt takes the division — protocol is a cell, the kernel only owns frames and
interrupts — because it is the division the single-address-space model already
enforces for storage. What Molt cannot take is seL4's IOMMU-backed isolation of
the NIC's DMA; `docs/pci.md` already records that without an IOMMU a
bus-mastering device is as privileged as the kernel, and that trade is Stage 4's.

**Theseus** puts drivers and stack in the same single address space Molt uses,
with safety from Rust types rather than page-table isolation, and treats a NIC
queue as a typed, owned structure. This is the closest match to Molt's
constraints and the clearest evidence the approach holds: a `no_std` stack whose
buffers are typed and owned, no `unsafe` pointer arithmetic in the protocol
layer, and completion through the same mechanism the rest of the OS uses.

**Redox** contributes the namespace, which Molt already took: a scheme is a
service, a socket is acquired through it, and `smoltcp` sits behind Redox's
`netstack` as the protocol engine. Redox validates the split Molt plans — a thin
scheme boundary in front of a protocol library — and validates smoltcp as that
library.

**smoltcp** is a `no_std`, allocation-optional TCP/IP stack: Ethernet, ARP,
IPv4/IPv6, ICMP, UDP, and TCP, driven by a `Device` trait that hands it frames.
Its `Device` trait is almost exactly the `Link` seam above. The decision it
forces is below.

## smoltcp: UDP now, or TCP later

UDP is small — parse/emit IPv4 and a UDP header, checksum, demux by port — and
writing it against Molt's own buffers keeps the datagram path in types the
kernel already checks, with no dependency to audit and no allocation smoltcp
sometimes wants. TCP is not small: congestion control, retransmission, reassembly,
and timers are the part nobody should reimplement to learn nothing.

So: Molt writes UDP (and the Ethernet/ARP/IPv4 beneath it) itself, and smoltcp
becomes the engine inside `tcp-cell` when TCP arrives, fed by the same `Link`.
The issue's own read — "smoltcp maybe later as a TCP backend" — is the plan.
This keeps the `udp-cell` free of a large dependency for a protocol that does not
need it, and reserves the dependency for the protocol that earns it. Both cells
present the same registry scheme and the same ring protocol, so a client does
not learn which one answers.

## The plan, smallest step first

Each sub-stage is the least the next cannot proceed without, mirroring Stage 2.

1. **VirtIO-net over the existing transport.** A second device type on
   `molt-virtio`'s queue, receive and transmit queues over DMA regions, a `Link`
   trait it implements and a loopback implements for host tests. `MOLT_NET_OK`.
2. **Ethernet + ARP.** Frame parse/emit and an address cache, tested over the
   loopback `Link` with no hardware. Failing closed on a frame that does not
   parse, the way the audit fails closed on a leaf it cannot classify.
3. **IPv4.** Header parse/emit, checksum, fragmentation refused rather than
   mishandled at first, one route. IPv6 is the same seam with a second address
   type, deferred but not designed out.
4. **UDP + `udp-cell`.** Port demux, a datagram as a ring op addressed by
   `Capability<Socket>`, the cell published under a `Net` scheme and restartable
   with handles revoked. `MOLT_UDP_OK` when a datagram round-trips through QEMU's
   virtio-net device.
5. **TCP + `tcp-cell`.** smoltcp behind the same `Link` and the same scheme.

## What is deliberately not here

- **No sockets in the kernel.** A socket is a cell's capability, not a kernel
  descriptor, for the reason MoltFS gives about files.
- **No allocation in the datagram path.** UDP buffers are DMA regions the kernel
  owns, consistent with `molt-core`'s no-alloc rule; smoltcp's optional
  allocation stays inside `tcp-cell`, where a heap already exists.
- **No IOMMU assumption.** The NIC's DMA is as trusted as the disk's until Stage
  4 supplies device isolation; this is stated, not silently assumed.
- **No polling.** Receive is an interrupt awaited through `InterruptSlab`, the
  same path virtio-blk completions already take.
