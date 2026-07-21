//! Enumerating the bus, and taking one interrupt from it.
//!
//! This is the composition root for Stage 2.2: the only place that holds a
//! platform, a configuration-space window, and the interrupt slab at once. The
//! crates below it each know one of those three and none of them know each
//! other, which is the property the layering exists to keep.
//!
//! The smoke is deliberately end-to-end rather than a set of unit assertions.
//! Every piece here has host tests already; what no host test can show is that
//! a window mapped by the platform, a capability found by molt-pci, and a
//! vector minted by the interrupt fabric all describe the same device — and the
//! only proof of that is an interrupt actually arriving.

use core::fmt::Write;

use molt_arch::memory::{Inventory, Rights};
use molt_arch::{BootInfo, Mmio, Platform, SerialWriter, Sink};
use molt_core::interrupt::{InterruptSlab, InterruptToken};
use molt_pci::{Bus, Command, Function, bus_span, preferred};

/// One line per vector the platform's interrupt bank holds.
const LINES: usize = 16;

/// The QEMU teaching device: a PCI function that exists to be poked.
///
/// It is the one device on the machine whose interrupt can be raised on demand
/// from software, which is what makes a deterministic delivery test possible at
/// all — a disk or a NIC raises interrupts when it feels like it, not when a
/// smoke test would like one.
const EDU_VENDOR: u16 = 0x1234;
const EDU_DEVICE: u16 = 0x11e8;

/// Raise and acknowledge, in `edu`'s BAR 0. Writing a bit pattern to the first
/// asserts the interrupt; writing it back to the second clears it.
const EDU_RAISE: u64 = 0x60;
const EDU_ACKNOWLEDGE: u64 = 0x64;
const EDU_PATTERN: u32 = 1 << 8;

/// How long to spin for the arrival before calling the path broken.
///
/// A spin count rather than a timer deadline: an interrupt path wedged badly
/// enough to lose this message could equally well have lost the timer tick that
/// was supposed to bound the wait, and a smoke test that hangs tells nobody
/// anything.
const DELIVERY_SPINS: u32 = 10_000_000;

/// The interrupt slab, with the adapter that lets interrupt context reach it.
///
/// A newtype rather than an `impl Sink for InterruptSlab` inside molt-core: the
/// slab is a data structure, the sink is a wiring decision, and keeping them
/// apart is what lets a kernel that wants two banks have two.
struct Interrupts(InterruptSlab<LINES>);

impl Sink for Interrupts {
    fn raise(&self, line: u16) {
        self.0.raise(line);
    }
}

static INTERRUPTS: Interrupts = Interrupts(InterruptSlab::new());

macro_rules! report {
    ($platform:expr, $($arg:tt)*) => {{
        let _ = writeln!(SerialWriter::new($platform.serial()), $($arg)*);
    }};
}

/// A device found during the walk, routed and ready to be provoked.
struct Routed {
    registers: Mmio<'static>,
    token: InterruptToken,
}

/// Enumerates bus zero and, where the platform can deliver one, takes an MSI.
///
/// Firmware that describes no configuration space is reported and skipped
/// rather than fatal. A kernel that panicked over it would be asserting a fact
/// about the machine it happens to be on, not about itself.
pub fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let Ok(space) = platform.config_space(boot_info) else {
        report!(platform, "MOLT_PCI_SKIPPED: firmware described no configuration space");
        return;
    };

    let inventory = Inventory::new(boot_info.memory_map());
    let bus_zero = bus_span(space, space.first_bus()).expect("bus zero inside the ECAM window");
    let device = inventory.device(bus_zero).expect("the ECAM window is not memory the kernel owns");
    let window = platform
        .map_device(device, Rights::READ_WRITE)
        .expect("a device window the platform can map");

    let (found, routed) = enumerate(platform, &inventory, &window);
    report!(platform, "MOLT_PCI_OK: {found} functions on bus {}", space.first_bus());

    match routed {
        Some(routed) => deliver(platform, &routed),
        None => report!(platform, "MOLT_MSI_SKIPPED: no routable device on this machine"),
    }
}

/// Walks the bus once, sizing BARs and routing `edu` if it is there.
///
/// One walk rather than two because a [`Function`] borrows the bus window:
/// there is nowhere to set one aside for later. That is the borrow doing its
/// job — a function handle outliving its mapping would be a dangling MMIO
/// pointer, and the type system will not build one.
fn enumerate<P: Platform>(
    platform: &mut P,
    inventory: &Inventory<'_>,
    window: &Mmio<'_>,
) -> (u32, Option<Routed>) {
    let mut bus = Bus::new(window, 0);
    let mut found = 0;
    let mut sized = false;
    let mut routed = None;

    while let Some(mut function) = bus.function() {
        found += 1;
        let address = function.address();
        report!(
            platform,
            "MOLT_PCI: {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}",
            address.bus(),
            address.device(),
            address.function(),
            function.vendor(),
            function.device(),
            function.class().class(),
        );

        if !sized {
            sized = size_first_bar(platform, &mut function);
        }
        if routed.is_none() && function.vendor() == EDU_VENDOR && function.device() == EDU_DEVICE {
            routed = route(platform, inventory, &mut function);
        }
    }

    assert!(found > 0, "an ECAM window that answers with no function at all");
    if !sized {
        // A machine whose bus carries only a host bridge has nothing to size.
        // That is a fact about the board, not a failure of the decoder, and the
        // decoder's arithmetic is covered by host tests either way.
        report!(platform, "MOLT_BAR_SKIPPED: no function on bus zero implements a memory BAR");
    }
    (found, routed)
}

/// Reports the first memory BAR the function implements.
///
/// Sizing is destructive — it writes all-ones and restores — so this stops at
/// the first one that answers rather than poking every device on the bus.
fn size_first_bar<P: Platform>(platform: &mut P, function: &mut Function<'_>) -> bool {
    for index in 0..6 {
        let Ok(Some(bar)) = function.bar(index) else { continue };
        if !bar.is_memory() {
            continue;
        }
        report!(
            platform,
            "MOLT_BAR_OK: bar {} at {:#x} for {:#x} bytes",
            bar.index(),
            bar.base(),
            bar.bytes(),
        );
        return true;
    }
    false
}

/// Binds a line, programs the device with the fabric's message, and maps the
/// registers the interrupt will be raised through.
///
/// A platform whose fabric has no vectors — RISC-V without an AIA — returns
/// `None` here and the smoke says so. Refusing to boot would make an interrupt
/// controller a requirement for enumerating a bus, which it is not.
fn route<P: Platform>(
    platform: &mut P,
    inventory: &Inventory<'_>,
    function: &mut Function<'_>,
) -> Option<Routed> {
    let capability = preferred(function).ok()?;
    let (line, message) = platform.allocate().ok()?;

    // The line is bound before the device is programmed, never after: a device
    // able to deliver into a line nobody owns is a dropped interrupt at best.
    let token = INTERRUPTS.0.bind(line).expect("a line the fabric just handed out");
    platform.route_interrupts(&INTERRUPTS).expect("a platform that hands out vectors delivers");

    // `edu` implements MSI, not MSI-X: its table would need a BAR mapping of
    // its own, and `preferred` picking MSI-X here would be a different path.
    let msi = function.msi().expect("the capability `preferred` selected");
    function.route_msi(msi, message).expect("a capability at the offset it reported");
    report!(
        platform,
        "MOLT_MSI_OK: line {line} vector {:#x} at {:#x} via capability {:#x}",
        message.data(),
        message.address(),
        capability.offset(),
    );

    // The BAR is classified against the same firmware map the kernel's own RAM
    // came from. A device claiming a base inside RAM is refused here — which is
    // the check that stops a misprogrammed BAR from becoming a write into the
    // kernel through an uncached window.
    let bar = function.bar(0).ok()??;
    let span = bar.span().ok()?;
    let window = inventory.device(span).ok()?;
    let registers = platform.map_device(window, Rights::READ_WRITE).ok()?;

    // Memory decode goes on last: by now the vector is routed and the registers
    // are mapped, so the first access the device can answer has somewhere to go.
    let command = function.command().ok()?;
    function.set_command(command.with(Command::MEMORY)).ok()?;

    Some(Routed { registers, token })
}

/// Raises the device's interrupt and waits for the slab to count it.
///
/// Polling rather than awaiting: this runs before there is an executor, and
/// what is being proven is that the arrival reaches the slab at all. The future
/// on top of the same counter is what the driver will use.
fn deliver<P: Platform>(platform: &mut P, routed: &Routed) {
    let before = INTERRUPTS.0.arrivals(routed.token).expect("a line this kernel bound");
    assert_eq!(before, 0, "an arrival before anything raised one");

    routed.registers.write_u32(EDU_RAISE, EDU_PATTERN).expect("the device's raise register");

    let mut spins = 0;
    let arrivals = loop {
        match INTERRUPTS.0.arrivals(routed.token) {
            Ok(0) => {}
            Ok(arrivals) => break arrivals,
            Err(error) => panic!("the bound line went stale: {error:?}"),
        }
        spins += 1;
        assert!(spins < DELIVERY_SPINS, "the device raised an interrupt that never arrived");
        core::hint::spin_loop();
    };

    routed
        .registers
        .write_u32(EDU_ACKNOWLEDGE, EDU_PATTERN)
        .expect("the device's acknowledge register");
    report!(platform, "MOLT_INTERRUPT_OK: {arrivals} arrival after {spins} spins");
}
