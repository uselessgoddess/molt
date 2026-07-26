# Networking

Status: Stage 3 implementation record, July 2026.

Molt now carries packets through VirtIO-net, Ethernet, ARP, IPv4, and UDP. TCP
remains the next protocol. This is the decision and implementation record —
what the stack borrows from seL4, Theseus, Redox, and smoltcp, what it
deliberately does not, and how it lands on the primitives the kernel already
has: DMA arenas, paired rings, typed capabilities, cells, and the registry.

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
filesystem talks to without seeing a virtqueue. Networking uses the same seam:
`molt-net::Link` moves frames in and out, `molt_virtio::Net` implements it, and
a loopback implements it for host tests. Protocol code is therefore tested off
hardware the way MoltFS is tested over a loopback disk.

## The kernel is not tied to a protocol

The issue's framing holds: there is no UDP abstraction in the kernel. The
`molt-udp` service binds ports, answers on a ring, and revokes its socket
capabilities when its `UdpCell` restarts. `molt-net` owns Ethernet, ARP, IPv4,
and protocol capabilities. The kernel below them maps the device, routes its
interrupt, and moves frames; it owns nothing about ports.

This is the same move MoltFS made: the kernel has no `open(2)` and no path
resolver, only rings and capabilities, and the filesystem is a cell built out
of those. A socket is likewise not a kernel object. A datagram is a ring
operation addressed by `Capability<Socket>`, with no ambient port table and no
global namespace — the same argument `docs/fs.md` makes about what *typed*
buys, applied to the network. Registry publication is the remaining discovery
step; the service and restart boundary do not depend on it.

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
can present the same registry scheme and ring protocol, so a client need not
learn which one answers.

## The implemented path

Each layer is the least the next needs, mirroring Stage 2:

```text
MSI-X -> InterruptSlab -> VirtIO RX queue -> molt-net IP ring
                                           -> molt-udp socket ring
```

1. **VirtIO-net over the existing transport.** `molt_virtio::Net` requires the
   modern transport, a stable MAC address, and the complete receive-header
   format. It gives RX queue zero and TX queue one a shared MSI-X table entry,
   fills every receive descriptor before `DRIVER_OK`, and reposts a used
   receive buffer before returning its frame.
   No checksum or segmentation offload is negotiated. The merged-buffer format
   is required only to make the modern 12-byte header explicit; because no
   guest segmentation feature is accepted, each 1526-byte buffer holds one
   complete maximum-size frame and `num_buffers` must remain one.
2. **Ethernet + ARP.** `molt-net` parses and emits bounded frames and keeps a
   fixed neighbor table. An unresolved next hop emits ARP and leaves the IP send
   pending; the reply retries it without blocking or allocating.
3. **IPv4.** The service checks header and payload checksums, has one static
   route, and refuses fragments rather than accepting data it cannot reassemble.
   A protocol number can be bound by only one live capability.
4. **UDP + `UdpCell`.** `molt-udp` owns port demultiplexing and socket
   capabilities. Payloads cross its ring only through registered buffers; its
   private registered scratch buffers carry the nested IP-ring operations.
   Restart revokes the old generation's sockets.
5. **Hardware proof.** The x86_64 smoke attaches modern VirtIO-net to QEMU's
   user network. It resolves the gateway by ARP and sends a DNS request to the
   user-network DNS proxy. `MOLT_NET_OK` proves device startup and
   `MOLT_UDP_OK` requires a checked DNS response to traverse the device, IP
   ring, and UDP ring.

TCP remains a separate `tcp-cell`; smoltcp can sit behind the same `Link`
without changing the device or capability boundary.

## What is deliberately not here

- **No sockets in the kernel.** A socket is a cell's capability, not a kernel
  descriptor, for the reason MoltFS gives about files.
- **No allocation in the datagram path.** Device frames come from a DMA arena,
  and protocol operations use fixed arrays plus registered buffers, consistent
  with `molt-core`'s bounded rings. smoltcp's optional allocation stays inside
  `tcp-cell`, where a heap already exists.
- **No IOMMU assumption.** The NIC's DMA is as trusted as the disk's until Stage
  4 supplies device isolation; this is stated, not silently assumed.
- **No device polling in the network path.** Receive work is drained only after
  `InterruptSlab` reports MSI-X activity. The older block smoke still uses its
  explicitly bounded polling path; networking does not copy that exception.
