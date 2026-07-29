use core::sync::atomic::{AtomicU16, Ordering};

use molt_arch::memory::{Inventory, Rights};
use molt_arch::{BootInfo, CpuId, Mmio, MsiMessage, Platform, SerialWriter, Sink};
use molt_core::interrupt::{InterruptSlab, InterruptToken};
use molt_exec::Handle;
use molt_kernel::report;
use molt_pci::{Bus, Command, Function, bus_span, preferred};

const LINES: usize = 16;

/// Stands for a line no interrupt has arrived on yet.
const NOBODY: u16 = u16::MAX;

/// The QEMU teaching device: a PCI function that exists to be poked.
const EDU_VENDOR: u16 = 0x1234;
const EDU_DEVICE: u16 = 0x11e8;
const EDU_RAISE: u64 = 0x60;
const EDU_ACKNOWLEDGE: u64 = 0x64;
const EDU_PATTERN: u32 = 1 << 8;

/// How long to wait for the arrival before calling the path broken.
const DELIVERY_TICKS: u64 = 64;

/// The device lines, and which core each one was last delivered to.
///
/// A vector aimed at one core and taken by another is the failure the affinity
/// is meant to rule out, so the core writes down that it was here.
struct Interrupts {
    slab: InterruptSlab<LINES>,
    served: [AtomicU16; LINES],
}

impl Sink for Interrupts {
    fn raise(&self, line: u16) {
        let _heap = crate::heap::interrupt();
        if let Some(served) = self.served.get(line as usize) {
            served.store(crate::smp::here() as u16, Ordering::Release);
        }
        self.slab.raise(line);
    }
}

static INTERRUPTS: Interrupts =
    Interrupts { slab: InterruptSlab::new(), served: [const { AtomicU16::new(NOBODY) }; LINES] };

/// Which core took the last interrupt on `line`.
fn served(line: u16) -> Option<CpuId> {
    let served = INTERRUPTS.served.get(line as usize)?.load(Ordering::Acquire);
    (served != NOBODY).then(|| CpuId::new(served))
}

/// Reserves one device line on the core that will serve it, and connects the
/// platform entry path to the slab.
pub(crate) fn bind<P: Platform>(
    platform: &mut P,
    cpu: CpuId,
) -> Option<(InterruptToken, MsiMessage)> {
    let (line, message) = platform.allocate(cpu).ok()?;
    let token = match INTERRUPTS.slab.bind(line) {
        Ok(token) => token,
        Err(_) => {
            let _ = platform.release(line);
            return None;
        }
    };
    if platform.route_interrupts(&INTERRUPTS).is_err() {
        let _ = INTERRUPTS.slab.release(token);
        let _ = platform.release(line);
        return None;
    }
    Some((token, message))
}

/// Takes the arrivals recorded since this owner last drained its line.
pub(crate) fn arrivals(token: InterruptToken) -> u64 {
    INTERRUPTS.slab.arrivals(token).expect("a live device interrupt token")
}

/// Awaits the next arrival on `token`, giving up after `ticks`.
///
/// Nothing spins: the core parks, the interrupt wakes it, and the wheel is what
/// says when to stop believing in the device. A deadline reads as no arrivals,
/// which is what the caller already treats as a wedged line.
pub(crate) fn wait(token: InterruptToken, ticks: u64) -> u64 {
    let exec = crate::smp::current();
    match exec.block_on(exec.timers().timeout(ticks, INTERRUPTS.slab.wait(token))) {
        Some(arrivals) => arrivals.expect("a live device interrupt token"),
        None => 0,
    }
}

/// Returns a stopped device's line to both the slab and the platform fabric.
pub(crate) fn release<P: Platform>(platform: &mut P, token: InterruptToken) {
    INTERRUPTS.slab.release(token).expect("the line this device bound");
    platform.release(token.line()).expect("a line this platform allocated");
}

/// A device found during the walk, routed and ready to be provoked.
struct Routed {
    registers: Mmio<'static>,
    token: InterruptToken,
    /// The core the vector was aimed at, when it is not this one.
    home: Option<Handle>,
}

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
        report!(
            platform,
            "MOLT_PCI: {} {:04x}:{:04x} class {:02x}",
            function.address(),
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
    // The vector goes to the core that will serve it, and the driver's wait
    // goes with it. A peer is picked when one is up so the affinity is a claim
    // the smoke can fail on, rather than one every message satisfies by being
    // aimed at the only core there is.
    let home = crate::smp::peers().next().map(|(_, handle)| handle);
    let cpu = home.as_ref().map_or_else(|| platform.cpu(), Handle::cpu);
    // The line is bound before the device is programmed, never after: a device
    // able to deliver into a line nobody owns is a dropped interrupt at best.
    let (token, message) = bind(platform, cpu)?;
    let line = token.line();

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
    //
    // Bus mastering goes on with it, and it is worth being blunt about why: an
    // MSI *is* a DMA write. The device posts the message to `0xfee0_0000`, and
    // a function that may not initiate transactions cannot post it — QEMU drops
    // the write into a disabled bus-master address space and the interrupt
    // simply never happens. So there is no such thing as "MSI without bus
    // mastering", and pretending otherwise would be a comforting lie in the
    // capability model. Granting it here grants this device the whole physical
    // address space until there is an IOMMU, which is why the kernel does it in
    // one visible place, for one device it chose, rather than in `molt-pci`
    // where every caller would inherit it.
    let command = function.command().ok()?;
    function.set_command(command.with(Command::MEMORY).with(Command::BUS_MASTER)).ok()?;

    Some(Routed { registers, token, home })
}

/// Raises the device's interrupt and waits for the slab to count it.
///
/// The wait is the driver's: the same future on the same counter, parked on the
/// executor this core is already running.
fn deliver<P: Platform>(platform: &mut P, routed: &Routed) {
    let before = INTERRUPTS.slab.arrivals(routed.token).expect("a line this kernel bound");
    assert_eq!(before, 0, "an arrival before anything raised one");

    routed.registers.write_u32(EDU_RAISE, EDU_PATTERN).expect("the device's raise register");

    let arrivals = match &routed.home {
        Some(home) => away(platform, home, routed.token),
        None => wait(routed.token, DELIVERY_TICKS),
    };
    assert_ne!(arrivals, 0, "the device raised an interrupt that never arrived");

    routed
        .registers
        .write_u32(EDU_ACKNOWLEDGE, EDU_PATTERN)
        .expect("the device's acknowledge register");
    report!(platform, "MOLT_INTERRUPT_OK: {arrivals} arrival");
}

/// Waits for the arrival on the core the vector was aimed at, not on this one.
///
/// The wait is spawned there and the count comes back over the ring that pair
/// alone shares. Both halves are checked: the core that answered is the one the
/// line was homed on, and the core that took the interrupt is the same again.
fn away<P: Platform>(platform: &mut P, home: &Handle, token: InterruptToken) -> u64 {
    let cpu = home.cpu();
    let exec = crate::smp::current();
    let (answers, asked) = crate::smp::ask(
        exec,
        core::iter::once((cpu.index(), home.clone())),
        DELIVERY_TICKS,
        move || async move {
            let arrivals = INTERRUPTS.slab.wait(token).await;
            (crate::smp::here(), arrivals.expect("a live device interrupt token"))
        },
    );
    assert_eq!(asked, 1, "the core the line was homed on took no task");
    let Some(&(awaited, arrivals)) = answers.first() else { return 0 };

    assert_eq!(awaited, cpu.index(), "the wait ran off the core the line was homed on");
    assert_eq!(served(token.line()), Some(cpu), "the vector landed on another core");
    report!(platform, "MOLT_AFFINITY_OK: line {} on core {}", token.line(), cpu.index());
    arrivals
}
