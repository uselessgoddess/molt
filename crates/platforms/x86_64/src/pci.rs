//! Configuration space, as this platform reaches it.
//!
//! `molt-pci` knows what the registers mean and nothing about where they are;
//! this module is the other half. It holds the one mapped window [`memory::init`]
//! built, turns a sweep of it into the reports the kernel sees, and — because a
//! message signalled interrupt is a write to an address only the interrupt
//! controller can name — is where a device's vector gets chosen and delivered.
//!
//! [`memory::init`]: crate::memory::init

use core::cell::UnsafeCell;

use molt_arch::{BootInfo, DeviceFunction, PlatformError};
use molt_pci::{Command, Ecam, Error, Function, Message, Table, msix, scan};

use crate::memory::Configuration;
use crate::{apic, interrupts, memory};

/// QEMU's education device: a BAR, an MSI capability, and a register that makes
/// it raise its vector on demand. It is the only device on either machine this
/// kernel runs on that can be made to interrupt without a driver behind it,
/// which is what makes end-to-end delivery observable at all.
const EDU: (u16, u16) = (0x1234, 0x11e8);

/// Writing here makes the education device raise its interrupt, with the value
/// written becoming its interrupt status.
const EDU_RAISE: u64 = 0x60;

/// Acknowledging clears the bits written back out of that status.
const EDU_ACKNOWLEDGE: u64 = 0x64;

struct Window(UnsafeCell<Option<Ecam>>);

// SAFETY: the window is installed on the boot CPU during `initialize`, before
// any other core exists, and is only read afterwards.
unsafe impl Sync for Window {}

static WINDOW: Window = Window(UnsafeCell::new(None));

/// Installs the mapped configuration window, if the firmware described one.
pub fn init(configuration: Option<Configuration>) {
    let Some(configuration) = configuration else { return };
    // SAFETY: `memory::init` mapped `[base, base + span)` read/write and
    // uncacheable over the aperture MCFG named, and nothing else maps it.
    let ecam = unsafe {
        Ecam::new(configuration.base as *mut u32, configuration.first, configuration.last)
    };
    // SAFETY: single boot CPU, and this runs once.
    unsafe { *WINDOW.0.get() = Some(ecam) };
}

/// The configuration window, or `None` on a machine with no ECAM aperture.
pub fn window() -> Option<&'static Ecam> {
    // SAFETY: the cell is written once during `initialize` and read-only after.
    unsafe { &*WINDOW.0.get() }.as_ref()
}

/// Every function the window's buses hold.
pub fn functions() -> impl Iterator<Item = Function<'static, Ecam>> {
    window().into_iter().flat_map(scan)
}

/// Reports every function behind the window, in address order.
///
/// A machine whose firmware described no configuration space is not an error:
/// there is nothing to enumerate, and saying so is the whole answer.
pub fn enumerate(found: &mut dyn FnMut(DeviceFunction)) -> Result<(), PlatformError> {
    for function in functions() {
        found(report(function));
    }
    Ok(())
}

/// What one function looks like from outside the bus crate.
fn report(function: Function<'static, Ecam>) -> DeviceFunction {
    let at = function.address();
    let id = function.id();
    let class = function.class();
    DeviceFunction {
        bus: at.bus(),
        device: at.device(),
        function: at.function(),
        vendor: id.vendor,
        id: id.device,
        class: class.class,
        subclass: class.subclass,
        // Measuring each window costs two writes to a register the device is
        // not decoding through at the time, which is why this is a boot-time
        // sweep and not something a driver repeats.
        windows: function.bars().count() as u8,
        // MSI-X first: a device that implements both is driven through the form
        // with a mask bit per vector.
        vectors: function
            .msix()
            .map(|msix| msix.vectors())
            .or_else(|_| function.msi().map(|msi| msi.vectors()))
            .unwrap_or(0),
    }
}

/// The first function reporting this vendor and device.
fn find(id: (u16, u16)) -> Option<Function<'static, Ecam>> {
    functions().find(|function| (function.id().vendor, function.id().device) == id)
}

/// Binds a device's MSI vector to the interrupt path and makes it fire.
///
/// This is the whole of "routed to the existing interrupt path", proved rather
/// than asserted: the kernel picks a vector, the device is told to write the
/// address the local APIC decodes into it, and then the device is poked through
/// a window of its own BAR. What arrives is a real interrupt on a real vector,
/// through the same descriptor table the timer uses.
pub fn raise_message_interrupt(boot_info: &BootInfo<'_>) -> Result<u8, PlatformError> {
    let function = find(EDU).ok_or(PlatformError::Unsupported)?;
    let msi = function.msi().map_err(hardware)?;
    let bar = function.bar(0).map_err(hardware)?;
    let vector = interrupts::allocate().ok_or(PlatformError::InvalidHardware)?;

    // Programming happens with delivery off, so the device cannot raise a
    // vector assembled out of half of one message and half of another.
    msi.program(Message::new(apic::message_address(), u32::from(vector))).map_err(hardware)?;
    // The wired interrupt is masked at both PICs and routed nowhere; a device
    // left able to raise it would deliver an edge nothing is waiting on.
    function.enable(Command::MEMORY | Command::BUS_MASTER | Command::INTX_DISABLE);
    let registers = memory::open_window(boot_info, bar.base(), bar.size())?;
    msi.enable();

    // SAFETY: `registers` maps this device's own BAR read/write and
    // uncacheable, and both offsets are naturally aligned 32-bit registers
    // inside it.
    unsafe {
        ((registers + EDU_ACKNOWLEDGE) as *mut u32).write_volatile(!0);
        ((registers + EDU_RAISE) as *mut u32).write_volatile(1);
    }
    Ok(vector)
}

/// Programs a real MSI-X table through a window of the BAR it lives in.
///
/// Delivery is not proved here, and cannot be by any device on this machine
/// that has a table: they all need a driver to have work outstanding before
/// they will raise anything. What is proved is the rest of the path — that the
/// capability names a BAR, that the BAR maps to memory the audit accepts, and
/// that an entry written through the mapping answers with what was written,
/// which no frame the kernel reached by accident would do.
pub fn verify_message_table(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let (function, msix) = functions()
        .find_map(|function| function.msix().ok().map(|msix| (function, msix)))
        .ok_or(PlatformError::Unsupported)?;
    let location = msix.table();
    let bar = function.bar(location.bar()).map_err(hardware)?;
    let vectors = msix.vectors();
    let vector = interrupts::allocate().ok_or(PlatformError::InvalidHardware)?;

    function.enable(Command::MEMORY | Command::INTX_DISABLE);
    // Turning delivery on masks every vector: the entries still hold whatever
    // was in that BAR at reset, and one of those is not an address.
    msix.enable();
    let base = memory::open_window(
        boot_info,
        bar.base() + u64::from(location.offset()),
        u64::from(vectors) * msix::ENTRY,
    )?;
    // SAFETY: `base` maps `vectors` entries of this device's own table
    // read/write and uncacheable, and nothing else in the kernel writes it.
    let table = unsafe { Table::new(base as *mut u32, vectors) };

    let message = Message::new(apic::message_address(), u32::from(vector));
    table.program(0, message).map_err(hardware)?;
    if table.message(0).map_err(hardware)? != message || !table.masked(0).map_err(hardware)? {
        return Err(PlatformError::InvalidHardware);
    }
    table.mask(0, false).map_err(hardware)?;
    if table.masked(0).map_err(hardware)? {
        return Err(PlatformError::InvalidHardware);
    }
    // Nothing waits on this vector, so the device is left unable to send it.
    table.mask(0, true).map_err(hardware)?;
    msix.disable();
    Ok(())
}

/// A device that does not answer the way its own capability said it would is a
/// hardware fault, not a kernel one.
fn hardware(_error: Error) -> PlatformError {
    PlatformError::InvalidHardware
}
