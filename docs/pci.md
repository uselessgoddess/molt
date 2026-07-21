# PCI and interrupts

Status: Stage 2.2 decision record, July 2026.

Why enumeration is a window rather than a bus object, why the message that
routes an interrupt is a type the driver cannot build, and what this stage
deliberately leaves undone. Written as the record for `molt-arch::mmio`,
`molt-arch::irq`, `molt-arch::pci`, `molt-core::interrupt`, and the `molt-pci`
crate.

## The shape of the problem

A PCI device is reached three different ways at once, and each way is a
different kind of authority:

- **Configuration space** says what the device is and where it decodes. It is
  memory-mapped, one 4 KiB window per function, at a base only firmware knows.
- **BARs** say where the device's registers live. The device reports them, so
  they are the one input on this path that hardware chooses and the kernel must
  not trust.
- **MSI** says how the device gets the CPU's attention. It is a store to an
  address that means something only to the interrupt controller.

Linux answers all three through one global `pci_dev` graph that every driver
can walk. That is the shape being avoided: a driver holding a `pci_dev` can
reach its neighbour's registers, and the only thing stopping it is that it does
not. Molt has no `pci_bus`, no `pci_get_device`, and no way to name a function
you were not given.

## Authority is the window

Everything in `molt-pci` operates on an
[`Mmio`](../crates/molt-arch/src/mmio.rs) window the platform already mapped.
The window *is* the authority: a caller who holds none cannot reach the bus,
and one holding a function's window cannot reach the function beside it,
because `Mmio::subwindow` only ever narrows.

That gives the borrow checker something real to enforce. `Bus::function`
returns a `Function<'bus>` borrowed from the bus window, `Function::bar`
borrows the function, and `MsiX` borrows both its control registers and the BAR
its table lives in — as two separate lifetimes, because they are two separate
mappings. A `Bar` handle outliving the mapping under it would be a dangling
MMIO pointer, and there is no way to write one down.

`Mmio` is also `Send` but deliberately not `Sync`. Two cores writing the same
device register interleaved is a driver bug that reads as a hardware fault, and
the type says so before it happens.

One consequence worth naming: `Bus` is not an `Iterator`. `Iterator::next`
cannot return an item borrowing the iterator, and rather than hand out an owned
handle with a raw pointer inside it — which is exactly the escape hatch this
design exists to avoid — `Bus::function` is a plain lending method.

## Where a physical address becomes something you may touch

Nothing in `molt-pci` maps memory. The chain is:

1. Firmware says where configuration space is —
   [`ConfigSpace`](../crates/molt-arch/src/pci.rs), read from an ACPI `MCFG`
   allocation on x86_64 and from a `pci-host-ecam-generic` node on RISC-V.
2. `bus_span` turns "bus 0 of that space" into a `Span`, one bus at a time. A
   whole segment is 256 MiB of window for what is usually a handful of
   functions, and a mapping that large is a large thing to get wrong.
3. [`Inventory::device`](memory.md) refuses a span that is not device memory.
4. The platform's `DeviceMapper` maps it, uncached and never executable.

Step 3 is the one that matters for BARs. A BAR is a device-supplied address; a
misprogrammed or hostile one naming a range inside RAM would otherwise become a
write into the kernel through an uncached window. `Inventory::device` refuses
it, and the kernel's smoke path classifies every BAR it maps.

`Inventory::device` was widened in this stage to accept `Kind::Reserved`
alongside `Kind::Device`. The reason is that the two firmwares describe the same
ECAM window differently: a device tree leaves it as a hole in the memory node,
while e820 lists it as an explicit reservation. `Kind::Ram` and `Kind::Image`
stay refused, which is the property the check exists for.

## BAR sizing, which is destructive by construction

A BAR does not report its size. It reports which of its address bits are
writable, and the only way to ask is to write all-ones and read back what
stuck — for the duration of which the register names a decode window at
whatever address the ones landed on.

So `Function::bar` turns memory and I/O decode off first, probes, restores the
register, and restores the command word on every path including the failing
ones. Leaving decode off would strand a device that was working before anyone
asked about its BARs.

The arithmetic is split out as `decode`, a pure function of two register
values. The destructive half needs real hardware to mean anything; the half
that is easy to get wrong needs none, and has host tests for the 32-bit case,
the 64-bit pair, the unimplemented BAR, and the 32-bit BAR whose upper half is
not a register at all and must not become 4 GiB of length.

## The message a driver cannot forge

`MsiMessage` is an opaque pair produced by the platform's `InterruptFabric` and
written to the device verbatim. `molt-pci` never computes one.

This is the whole reason it is a type rather than two `u32`s. On x86_64 the
address encodes a destination APIC ID and the data encodes a vector; a driver
able to assemble one could route a device at a vector the kernel is not
listening on, or at another core's. Since only the fabric mints them, the
device can only be pointed where the kernel already is.

`InterruptFabric::allocate` returns `(line, MsiMessage)`. The `line` is an
opaque `u16` — the same trick `Owner::Device(u32)` already uses to let
`molt-arch` name something `molt-core` owns without depending on it. `molt-arch`
sits *below* `molt-core`, so the delivery path is a `Sink` trait the kernel
implements:

```text
device --MSI--> APIC --vector--> handler --Sink::raise(line)--> InterruptSlab
```

`InterruptSlab` lives in `molt-core` and is where an arrival becomes something
a future can await. `raise` is the only method callable from interrupt context
and does exactly two things: bump a counter, wake a waker. Everything that
could block, allocate, or fault happens on the task side of `wait`.

Lines carry a generation. A line released and rebound gives the new owner a
fresh token, and the old token reports `InterruptError::Stale` rather than
reading a counter that now belongs to somebody else — the same discipline
`CompletionSlab` uses for cancelled requests, for the same reason.

## MSI or MSI-X

`preferred` picks MSI-X when the function has it. Not because it is newer:
MSI-X has an independent address and data per vector, and multiple *MSI*
vectors must be a contiguous power-of-two block whose low data bits the device
varies itself. A fabric that hands out one vector at a time cannot promise a
contiguous block, so `route_msi` requests exactly one vector and says so, and a
device needing several is expected to have MSI-X.

Two details in the MSI-X path are not obvious and are worth keeping:

- An entry is masked while it is written and unmasked afterwards. A device can
  sample a partially-written entry and deliver a message assembled from two
  different routes.
- `enable` clears the capability's global mask along with setting the enable
  bit, because otherwise the per-entry masks are not the thing deciding
  anything.

Both paths set `Command::INTX_DISABLE`. A device that can still assert its
legacy interrupt pin has a second path to the CPU that nothing is waiting on.

## The x86_64 vector bank

Sixteen vectors, `0x50..0x60`, installed in the IDT unconditionally at boot
rather than registered on demand. The handlers are generated by a macro over a
const generic line number, and each one is two lines: raise the sink, then EOI
unconditionally.

Unconditional EOI is the point. A stray interrupt on a line nobody bound must
still be acknowledged, or the local APIC's in-service register keeps that
priority level asserted and every equal-or-lower vector stops being delivered —
a hang whose cause is nowhere near where it shows up.

The sink is a `spin::Once<&'static dyn Sink>` rather than an atomic pointer,
because `&dyn Sink` is a fat pointer and a torn read of one is a jump through
the wrong vtable.

## What this stage does not do

**Bus mastering is not enabled anywhere.** A device with
`Command::BUS_MASTER` set can write anywhere in physical memory. Until there is
an IOMMU that is a trust decision, not a driver convenience, so the caller has
to ask for it in as many words — and Stage 2.3 will be the first place that
asks. This is a real limit, not a deferred nicety: on this kernel a DMA-capable
device is as privileged as the kernel.

**Device windows are never unmapped.** On both platforms `map_device` bumps a
cursor through a region of its own — `0xffff_9300_0000_0000` on x86_64,
`0x20_0000_0000` on RISC-V — and that cursor only ever moves forward. Nothing
in this stage releases a device, and an unmap that raced a driver still holding
its `Mmio` is precisely the bug the borrow on the window exists to prevent, so
the reclaim is left for the stage that first has a reason to reclaim. Each
window costs its own page-table frames out of a pool drained at boot; running
that pool dry is an `OutOfFrames`, not a fallback into fresh allocation, because
the only allocator still reachable at that point would hand back the frames the
live tables are built from.

Note what is *not* a limit any more. Stage 2.1 gave the kernel its own page
tables, whose direct map covers firmware-usable RAM only, so a device window no
longer has a write-back alias underneath it on x86_64 — `Audit::accepts` proves
it by refusing any mapping the kernel did not declare.

**RISC-V has no MSI fabric.** `InterruptFabric::allocate` returns
`FabricError::Unsupported` there, honestly, rather than inventing a message.
Sv39 without `Svpbmt` has no cacheability bits at all — uncached device
ordering comes from platform PMAs keyed on the physical address — and MSI needs
an AIA IMSIC the `virt` board's default configuration does not present. The
kernel prints `MOLT_MSI_SKIPPED` and carries on enumerating, because an
interrupt controller is not a prerequisite for reading a bus.

**Only bus zero is walked.** Bridges are enumerated as functions like anything
else, but their secondary buses are not descended into. Nothing in Stage 2
lives behind a bridge on either supported machine, and a walk with no consumer
is a walk with no test.

## How it is tested

The unit tests run against a fake configuration space — a byte array the same
`Mmio` type maps — so the capability walk, the BAR arithmetic, the MSI-X entry
protocol, and every refusal have host coverage under Miri.

What no host test can show is that a window mapped by the platform, a
capability found by `molt-pci`, and a vector minted by the fabric all describe
the same device. The only proof of that is an interrupt arriving, so the x86_64
smoke boots QEMU's `q35` machine with a `-device edu` attached, routes its MSI,
writes the pattern that raises it, and requires `MOLT_INTERRUPT_OK` on the
serial line.

`edu` is the choice because it is the one function on the machine whose
interrupt can be raised on demand from software. A disk or a NIC raises
interrupts when it feels like it, not when a smoke test would like one.

`q35` rather than the default `pc` machine because only the former's chipset
publishes an `MCFG` table. On `pc` there is no configuration space to find, the
kernel reports `MOLT_PCI_SKIPPED`, and the test would pass by doing nothing.

The RISC-V smoke enumerates the `virt` board's ECAM and stops there, which is
what its fabric can honestly support.
