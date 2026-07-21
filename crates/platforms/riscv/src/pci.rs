//! Configuration space, as this platform reaches it.
//!
//! The x86_64 half of this asks firmware where the bus is and gets an answer
//! out of a table; here the answer comes out of the device tree, and the two
//! meet at the same place: one mapped window, one [`Ecam`], and the same sweep
//! over it. Nothing below this line is architecture-specific — `molt-pci` reads
//! the same registers on both machines — which is the point of asking the
//! platform where configuration space is instead of what a device is.
//!
//! What this half deliberately does *not* do is deliver a message interrupt.
//! The `virt` board's interrupt file is AIA's IMSIC, which is a different
//! controller with a different address to write and a different way of naming a
//! vector; claiming a vector through the wired PLIC path instead would prove
//! nothing about MSI. Until that controller is implemented, this platform
//! reports [`PlatformError::Unsupported`] and the kernel treats it as a fact
//! about the machine rather than a failure.

use core::cell::UnsafeCell;

use molt_arch::{BootInfo, DeviceFunction, PlatformError};
use molt_pci::{Ecam, Function, ecam, scan};

use crate::fdt::Region;
use crate::paging;

struct Window(UnsafeCell<Option<Ecam>>);

// SAFETY: the window is installed on the boot hart during `initialize`, before
// any other hart exists, and is only read afterwards.
unsafe impl Sync for Window {}

static WINDOW: Window = Window(UnsafeCell::new(None));

/// Maps the configuration space the device tree described and installs it.
///
/// A board that named no bus is not an error: there is nothing to map, and the
/// kernel finds out by enumerating nothing.
pub fn init(boot_info: &BootInfo<'_>, bus: Option<Region>) -> Result<(), PlatformError> {
    let Some(bus) = bus else { return Ok(()) };
    // The tree gives the aperture in bytes and the bus numbers follow from it:
    // one 1 MiB window per bus, and a region too small for one is a region this
    // kernel has misread rather than a bus with no functions.
    let buses = bus.size / ecam::BUS;
    let Some(last) = buses.checked_sub(1).filter(|_| buses > 0) else {
        return Err(PlatformError::InvalidHardware);
    };
    let last = u8::try_from(last).unwrap_or(u8::MAX);
    let span = Ecam::span(0, last);
    let base = paging::open_window(boot_info, bus.base, span)?;

    // SAFETY: `open_window` mapped `[base, base + span)` read/write over the
    // aperture the tree named, uncacheable because the window is device memory,
    // and nothing else maps it.
    let window = unsafe { Ecam::new(base as *mut u32, 0, last) };
    // SAFETY: single boot hart, and this runs once during `initialize`.
    unsafe { *WINDOW.0.get() = Some(window) };
    Ok(())
}

/// The configuration window, or `None` on a board that described no bus.
fn window() -> Option<&'static Ecam> {
    // SAFETY: the cell is written once during `initialize` and read-only after.
    unsafe { &*WINDOW.0.get() }.as_ref()
}

/// Reports every function behind the window, in address order.
pub fn enumerate(found: &mut dyn FnMut(DeviceFunction)) -> Result<(), PlatformError> {
    let Some(window) = window() else { return Err(PlatformError::Unsupported) };
    for function in scan(window) {
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
