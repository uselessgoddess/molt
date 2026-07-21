# The bus and its interrupts

Status: Stage 2.2 decision record, July 2026.

Stage 2.1 ended with one device window, opened through `Inventory::device` and
audited in the live page tables. This stage asks the next question: where does
a kernel get the *list* of devices, and how does one of them wake it up. The
answers are a crate that knows what the registers mean, two platform modules
that know where they are, and a vector that lands in the interrupt path Stage
1.5 already built.

## What is a bus, and what is a machine

Configuration space is the same on both platforms. A function's vendor and
device identifiers sit at the same offsets, capabilities chain the same way,
an MSI-X table is described by the same three fields. What differs is only
*where the window is* and *what an interrupt controller decodes*.

So `molt-pci` is a zero-dependency crate that never maps anything, never reads
a control register, and never names a vector. It is handed a `Config` — a
32-bit window onto one segment — and produces `Address`, `Function`, `Bar`,
`Capability`, `Msi`, `MsiX`. It cannot produce a pointer, which is the point:
if it could, it could produce one the audit never saw.

The platform crates supply the other half, and they supply it differently
because the machines differ in exactly one place:

- **x86_64** asks firmware. ACPI's MCFG names the aperture and the bus range,
  which is why `acpi.rs` exists and why a machine without an ECAM bridge —
  i440fx, for instance — reports no window rather than guessing one.
- **RISC-V** asks the device tree. There is no architectural address for
  anything on this board: not for configuration space, not for the UART, not
  for RAM. The previous stage leaves a pointer in `a1`, `fdt.rs` walks the
  structure block once, and the node claiming to be `pci-host-ecam-generic`
  says where the bus is.

Both then call the same `open_window`, which is the same audited path the UART
already came through, and hand the result to the same `Ecam`.

Matching on `compatible` rather than on a node's name is deliberate. A name is
a label the board author chose; `compatible` is a contract. A bridge that says
`pci-host-ecam-generic` is one whose registers this kernel knows how to read,
whatever the board decided to call the node.

## Sweep, not walk

Enumeration sweeps every address in the bus range the window maps. The usual
alternative is a bridge walk: start at bus zero, read each bridge's secondary
and subordinate bus numbers, recurse.

The walk believes the bridges. A misprogrammed secondary-bus number makes it
descend into a bus that is not there, or loop. The sweep asks the same question
of every address the platform already mapped and audited, finds the same
functions, cannot loop, and needs no trust it does not already have. It costs
one configuration read per address that does not answer, at boot, once.

Nothing is allocated: the scan is an iterator, and each item borrows the window
it was found through.

## Reading a window costs a write

`DeviceFunction::windows` counts a function's BARs, and counting them means
writing all-ones to each and reading the size back. That is a write to a
register the device is not decoding through at the time, and it is restored
immediately — but it is still a write, which is why it happens once during the
boot-time sweep and is not something a driver repeats.

## An interrupt is a memory write

An MSI is a posted write and nothing else. The device is told an address and a
payload; the interrupt controller decodes them into a vector. Nothing in
`molt-pci` knows what either value means — `Message` is opaque there, and the
PCI side only copies it into a capability or a table.

On x86_64 the address is the local APIC's message region and the payload is
the vector, which the platform allocates out of a bank above the exception
range and points at the same descriptor table the timer uses. The device is
then enabled with `INTX_DISABLE`, because a device left able to raise its wired
interrupt would deliver an edge nothing is waiting on.

Two orderings in that path are load-bearing:

- **Program with delivery off.** The message is written before `msi.enable()`,
  so the device cannot raise a vector assembled out of half of one message and
  half of another.
- **Arm before poking.** The kernel claims the slab slot and polls the watch
  future *once* — asserting `Pending` — before it tells the device to
  interrupt. An interrupt that beats the first real poll is still observed.
  This is the reason binding and firing are separate calls on `Platform` rather
  than one convenient `fire_and_wait`.

`InterruptSlab::claim` exists for the same reason. Which number a device
interrupts on is not the slab's to decide: the vector comes from the platform
and the trap handler has nothing but that number to report, so a driver claims
*that* slot. `reserve` is now the special case of not caring which.

## What is proved, and where

| marker | what it establishes |
| --- | --- |
| `MOLT_PCI:` | one line per function found, with its identifiers, BAR count, and vector count |
| `MOLT_PCI_OK` | the sweep found functions, and at least one reported a message vector |
| `MOLT_MSI:` | a vector was bound, the device was poked, and the interrupt arrived |
| `MOLT_MSI_OK` | delivery completed end to end through the kernel's own slab |
| `MOLT_MSIX_OK` | a real MSI-X table was mapped and an entry read back what was written |

`MOLT_MSI_OK` needs a device that will interrupt with no driver behind it, and
QEMU's education device is the only one on either machine that will. That is
why the x86_64 smoke run adds `-device edu` and `-device virtio-rng-pci`: one
proves delivery, the other has a table at an offset inside a BAR that the
kernel then has to map.

`MOLT_MSIX_OK` deliberately proves less than delivery. Every device with a
table on these machines needs a driver with work outstanding before it will
raise anything. What is proved instead is the rest of the path: that the
capability names a BAR, that the BAR maps to memory the audit accepts, and that
an entry written through the mapping answers with what was written — which no
frame the kernel reached by accident would do.

The device-tree reader is tested where it can be tested properly: on the host,
against hand-built v17 trees. A misread tree sends every later window at the
wrong physical address, and "it booted under QEMU" is not evidence about a tree
QEMU did not produce. So there are tests for a node found by what it claims to
be, for a range decoded with the widths its *parent* declared, for a matching
node that has children, and for bytes that are not a tree being refused rather
than read.

## Limits

- **RISC-V delivers no message interrupt.** The `virt` board's interrupt file
  is AIA's IMSIC: a different controller, a different address to write, and a
  different way of naming a vector. Claiming a vector through the wired PLIC
  instead would prove nothing about MSI, so this platform reports
  `PlatformError::Unsupported` and the kernel treats that as a fact about the
  machine rather than a failure. The enumeration half is complete there; the
  delivery half waits for an IMSIC.
- **One segment.** Both platforms map the first aperture their firmware or
  their tree describes. Multi-segment machines exist; none that this kernel
  boots on does, and a second segment is a second window through the same call.
- **The device tree is read once and dropped.** Only the region is copied out,
  because the tree sits in RAM the frame allocator is about to hand out. A
  later consumer that wants more from it — `Svpbmt`, the timebase frequency,
  the interrupt controller — has to read it in the same window, before
  allocation starts, or the tree has to be reserved first.
- **Vectors are a flat bank.** The platform hands out vectors in order and
  never reclaims one. There is no unbinding, because nothing unbinds yet.
- **No IOMMU, so bus mastering is trust.** `Command::BUS_MASTER` is set on the
  bound device, and a device with that bit can write anywhere. Stage 2.0 said
  DMA is tracked and not contained; that is still the honest statement.

## References

- [PCI Express Base Specification](https://pcisig.com/specifications) — ECAM
  addressing, capability lists, MSI and MSI-X
- [Devicetree Specification v0.4](https://www.devicetree.org/specifications/)
  — the flattened format, `compatible`, and `#address-cells`
- [RISC-V AIA specification](https://github.com/riscv/riscv-aia) — the IMSIC
  the RISC-V delivery half is waiting on
