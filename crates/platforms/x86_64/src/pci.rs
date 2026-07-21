//! Configuration space, as this platform reaches it.

use core::cell::UnsafeCell;

use molt_pci::{Address, Ecam, Function, scan};

use crate::memory::Configuration;

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

/// The function at `at`, if one answers there.
pub fn function(at: Address) -> Option<Function<'static, Ecam>> {
    Function::probe(window()?, at).ok()
}

/// Every function the window's buses hold.
pub fn functions() -> impl Iterator<Item = Function<'static, Ecam>> {
    window().into_iter().flat_map(scan)
}
