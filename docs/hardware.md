# Real hardware

Status: Stage 4 decision record, August 2026.

Molt has never run outside QEMU. The roadmap has carried "documented
real-hardware boot on one named x86_64 machine" unchecked since Stage 1, and
[`docs/testing.md`](testing.md) says why: "Molt has no boards and no serial
capture equipment yet. Until it does, QEMU is the honest limit."

This document answers the buying question — which RISC-V board, at what price,
and is it worth the money now — by first asking what a board would actually
prove, because that turns out to decide the answer.

## What a RISC-V board would exercise

The smoke runner requires the same fifteen markers on both architectures
(`BOOT_MARKERS`, `xtask/src/main.rs:20`), plus two that only RISC-V prints
(`arch_markers`, `xtask/src/main.rs:165`). On real silicon a board would have to
produce, for real, everything under:

- `MOLT_SBI_CONSOLE:` — the DBCN probe and its legacy fallback, against a
  vendor OpenSBI build rather than QEMU's;
- `MOLT_UART_WINDOW:` — a device window mapped through `Inventory::device` at
  an address firmware chose;
- `MOLT_WX_OK` and `MOLT_MAPPING_OK` — the per-section Sv39 tables read back
  from the live `satp`, on an MMU that is not QEMU's;
- `MOLT_SMP_OK` — SBI HSM `hart_start` on as many harts as the board has;
- `MOLT_TIMER_OK`, `MOLT_EXCEPTION_OK` — `sstc` or the SBI timer, and a real
  trap path;
- the heap, frame ownership, executor, rings, and cells above them.

That is the whole portable claim of the kernel, and it is worth checking on
hardware: an ordering bug that x86_64 hides is exactly the class
[`docs/testing.md`](testing.md) built loom and the aarch64 job to catch, and
neither of those runs the kernel.

## What it would not exercise, and why that is decisive

Everything from Stage 2.2 onwards — PCI, VirtIO, NVMe, networking, the IOMMU —
would not run on any board in this price class, for three independent reasons.

**Molt's RISC-V port finds PCI in exactly one way.** `crates/platforms/riscv/src/fdt.rs:42`
matches a single `compatible` string:

```rust
/// The `compatible` string of the ECAM host bridge the kernel drives.
const ECAM: &[u8] = b"pci-host-ecam-generic";
```

QEMU's `virt` machine publishes that binding. Real SoCs do not. The StarFive
JH7110 (VisionFive 2, Milk-V Mars) is a PLDA XpressRICH controller behind
[`starfive,jh7110-pcie`](https://github.com/torvalds/linux/blob/master/Documentation/devicetree/bindings/pci/starfive%2Cjh7110-pcie.yaml);
the SpacemiT K1 (Orange Pi RV2, Banana Pi BPI-F3) is a Synopsys DesignWare
derivative with its own upstream host driver. On either board
`Platform::config_space` returns `Missing`, the kernel prints
`MOLT_PCI_SKIPPED`, and the smoke fails — `MOLT_PCI_OK` is required on both
architectures. That failure is correct behaviour, not a bug: the marker asserts
a property the machine has, which is the rule
[`docs/testing.md`](testing.md#boot-tests) already follows.

**No board in this class has an IOMMU.** Stage 4.5 and 4.6 are built on
VirtIO-IOMMU, which is a paravirtual device: it exists under QEMU and nowhere
else. The RISC-V IOMMU architecture is ratified —
[v1.0.1, 2026-02-22](https://docs.riscv.org/reference/iommu/index.html) — and
[Linux support was posted in 2024](https://lkml.iu.edu/hypermail/linux/kernel/2410.1/05516.html),
but none of the SoCs below documents one. Without it, `kernel/src/isolation.rs`
has nothing to open, and the isolation guarantee the whole device stack is
ordered around cannot be demonstrated at all.

**The load address is a QEMU constant.** `kernel/riscv64.ld` fixes
`ORIGIN = 0x80200000`, which is where OpenSBI hands off on `virt`. JH7110 boards
put DRAM at `0x4000_0000` — the VisionFive 2 device tree declares
[`memory@40000000`](https://patchwork.ozlabs.org/project/uboot/patch/20230118081132.31403-17-yanhong.wang@starfivetech.com/)
— so the link address is per-board today, not portable.

So a board buys the bottom third of the kernel and none of the top. The first
blocker is Molt's own code, not the price of silicon.

## The candidates

Prices are the lowest sourced figure found, before shipping and tax; RISC-V SBC
pricing moves and every one of these has been cheaper and dearer than the number
below.

| Board | SoC | Harts | RAM | Price | PCIe |
| --- | --- | --- | --- | --- | --- |
| [Milk-V Duo](https://milkv.io/duo) | SOPHGO CV1800B | 2× C906 (1 GHz / 700 MHz) | 64 MB | [$9](https://linuxgizmos.com/milk-v-duo-is-a-9-00-risc-v-tiny-embedded-computer/) | none |
| [Milk-V Duo S](https://milkv.io/duo) | SG2000 | 2× C906 + Arm | 512 MB | vendor lists no per-model price | none |
| [Orange Pi RV2](http://www.orangepi.org/html/hardWare/computerAndMicrocontrollers/details/Orange-Pi-RV2.html) | SpacemiT K1 (Ky X1) | 8 | 2–8 GB | [$30 / $39.90 / $49.90](https://www.cnx-software.com/2025/06/10/30-orange-pi-r2s-octa-core-risc-v-router-board-features-2x-2-5gbe-2x-gbe-2x-usb-ports/) | 2× M.2 M-key, Gen2 ×2 |
| [Milk-V Mars](https://milkv.io/mars) | StarFive JH7110 | 4 | 1–8 GB | ~$40–70 at retail | M.2 E-key |
| [VisionFive 2](https://www.waveshare.com/visionfive2.htm) | StarFive JH7110 | 4 | 4–8 GB | [$76 paid for 4 GB](https://bret.dk/risc-v-starfive-visionfive-2-review-jh7110/) | M.2 M-key, Gen2 ×1, NVMe boot |

The CNX figures are quoted for the Orange Pi R2S in an article that states
Orange Pi "kept the exact same prices for the R2S as for the earlier RV and RV2
SBCs"; Phoronix's
[RV2 benchmark piece](https://www.phoronix.com/review/orange-pi-rv2-benchmarks)
puts the 8 GB board under $100. The Mars range is retail listings rather than a
vendor price — [milkv.io/mars](https://milkv.io/mars) publishes none.

**"Works without extra peripherals" does not exist here.** Molt has no
framebuffer, no USB stack, and no keyboard; the console is a UART or the SBI
console. Every board on that list needs a 3.3 V USB-serial adapter and a way to
write an SD card, and the VisionFive 2 review describes needing UART *and* a
TFTP server *and* U-Boot surgery before an image would boot at all. The floor is
board plus dongle, roughly $5 on top.

**Two harts is not the interesting number.** Stage 4's whole design is
shared-nothing per core, and the Duo's second C906 is a small companion core
rather than a peer hart. `MOLT_SMP_OK` prints whatever HSM reports and the smoke
only requires the marker, so a two-hart board passes — it just does not stress
the thing that was designed. Eight harts on the RV2 would.

## How to test on a board conveniently

The runner is already most of the way there, and the part that is
QEMU-specific is small.

`smoke_case` builds a command, captures a serial stream, checks an exit status,
and then asserts markers with `run.serial.contains` — order-independent, which
is why `kernel/src/isolation.rs` could move a report line without touching the
runner. Only two of those four steps are about QEMU. A board transport would:

1. **Netboot, never touch the SD card.** Flash U-Boot once, then
   `tftpboot ${kernel_addr_r} molt.bin; go ${kernel_addr_r}` from a boot script.
   The kernel already takes `(a0 = hartid, a1 = device tree)` in S-mode, which is
   the SBI boot protocol U-Boot implements, so nothing in the entry path
   changes.
2. **Capture the console, not a pipe.** Open `/dev/ttyUSB0` at the board's rate
   and feed the same `case.markers()` and `arch_markers()` lists. The assertion
   code is reusable as-is.
3. **Power-cycle between runs.** A USB relay or a hub with per-port power
   switching is the difference between an unattended loop and a person pressing
   a button. This is the only new hardware the harness itself needs.
4. **Separate infrastructure failure from test failure.** seL4's lesson, already
   recorded in [`docs/testing.md`](testing.md#why-multi-platform-ci): a lab that
   cannot distinguish "the board did not come up" from "the code is wrong"
   trains everyone to ignore it. "No serial output at all within N seconds" is
   an infrastructure result and should be retried; a missing marker after a
   boot that produced other markers is a test failure and must not be.

One gap stays honest. On QEMU, `Platform::terminate` maps to an exit status the
runner checks (`check_exit_status`). On a board, SBI SRST reboots it, so there
is no status to read and the end of a run is `MOLT_BOOT_OK` plus a timeout. That
is weaker, and the doc should say so rather than pretend the two are the same
assertion.

And it stays a manual `just board`, never a CI gate. `docs/testing.md` already
argues the tiering: knowing a non-primary target broke is valuable, being unable
to merge until someone walks over to a board is not.

## Recommendation

**Do not spend on the $30–$70 class now.** Those boards are bought for their
PCIe, their NVMe slot, and their core count, and Molt can use none of it until
it has a non-ECAM PCI host driver. Buying one today converts a code problem into
the same code problem plus a parcel.

**Spend $9 plus a dongle, if anything.** A [Milk-V Duo](https://milkv.io/duo) is
the cheapest ready-made RV64 board that boots a real OpenSBI and a real U-Boot,
and the parts of Molt it can run — SBI console, Sv39 W^X, traps, timers, HSM,
heap, rings, cells — are exactly the parts whose correctness is currently
claimed rather than shown. It is a falsification device, not a development
platform, and $9 is the right price for one.

**The cheapest real-hardware result is not RISC-V at all.** The open roadmap
item is an x86_64 boot, the x86_64 port is the mature one — it is where every
device marker lives — and the hardware is any PC already on the desk plus a
serial dongle or a BMC with serial-over-LAN. That item is worth closing before
any board is ordered, because it is the one where a failure would be about Molt
rather than about a driver Molt has not written.

**What would change the answer.** Two things, in order:

1. Molt grows a device-tree PCI host path that is not `pci-host-ecam-generic`.
   Then an Orange Pi RV2 at $30 becomes the board to buy — eight harts for the
   shared-nothing design and an M.2 slot for the NVMe driver that already
   exists.
2. A board ships a ratified RISC-V IOMMU. Then the purchase is that board,
   whatever it costs, because it is the only way to run Stage 4.5's isolation
   argument on hardware that can enforce it — and everything above becomes a
   rehearsal for it.
