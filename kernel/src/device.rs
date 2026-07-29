//! What bringing a PCI device up takes, before any driver sees it.
//!
//! Both VirtIO functions the kernel starts want the same four things: their
//! BARs mapped, their structures cut out of one, a vector routed to a line the
//! slab owns, and that line given back once the device has stopped. Written
//! once here so the two smokes differ only in the device they find.

use molt_arch::memory::{Inventory, Rights};
use molt_arch::{Mmio, Platform};
use molt_core::interrupt::InterruptToken;
use molt_pci::{Bar, Function, MsiX, MsiXCapability, Vector};
use molt_virtio::{Arrivals, Location};

/// How long a driver waits on its line before calling the device wedged.
///
/// Ticks of the core's own quantum, so a couple of seconds: generous, because
/// what it has to outlast is a slow disk rather than a scheduler.
const WAIT_TICKS: u64 = 256;

/// Maps the BAR at `index` and reports where it landed.
pub(crate) fn map_bar<P: Platform>(
    platform: &mut P,
    inventory: &Inventory<'_>,
    function: &mut Function<'_>,
    index: u8,
) -> (Bar, Mmio<'static>) {
    let bar = function.bar(index).expect("a readable BAR").expect("an implemented BAR");
    let span = bar.span().expect("a frame-aligned BAR");
    let device = inventory.device(span).expect("a BAR outside kernel RAM");
    let mapping = platform.map_device(device, Rights::READ_WRITE).expect("a mappable BAR");
    (bar, mapping)
}

/// Cuts one VirtIO structure out of the BAR window it was reported in.
pub(crate) fn subwindow<'a>(registers: &'a Mmio<'_>, delta: u64, location: Location) -> Mmio<'a> {
    registers
        .subwindow(delta + location.offset() as u64, location.length() as u64)
        .expect("a VirtIO structure inside its BAR")
}

/// How far a BAR's base sits into the frames it was mapped with.
pub(crate) fn delta(bar: Bar) -> u64 {
    bar.base() - bar.span().expect("a frame-aligned BAR").start()
}

/// A device's MSI-X table, the line its vectors land on, and vector zero.
pub(crate) struct Vectored<'control, 'table> {
    msix: MsiX<'control, 'table>,
    token: InterruptToken,
    vector: Vector,
}

impl Vectored<'_, '_> {
    /// The entry index a driver programs its queue with.
    pub(crate) const fn index(&self) -> u16 {
        self.vector.index()
    }

    /// The arrivals a driver waiting on this vector sees.
    pub(crate) const fn line(&self) -> Line {
        Line { token: self.token, ticks: WAIT_TICKS }
    }

    /// Masks the vector, disables the capability, and returns the line.
    ///
    /// The device must already have been reset: a function still able to post
    /// a message into a released line is a stray write with an owner.
    pub(crate) fn stop<P: Platform>(mut self, platform: &mut P) {
        self.msix.mask(self.vector).expect("mask the stopped queue");
        self.msix.disable().expect("disable the stopped capability");
        crate::pci::release(platform, self.token);
    }
}

/// Routes vector zero of `function`'s MSI-X table to a line of its own.
///
/// The capability's registers live in configuration space and its table in a
/// BAR, so the two windows come from different mappings and are borrowed
/// apart. `table` is that BAR as it was mapped, `delta` how far its base sits
/// into it.
pub(crate) fn route<'control, 'table, P: Platform>(
    platform: &mut P,
    function: &'control Function<'_>,
    capability: MsiXCapability,
    table: &'table Mmio<'_>,
    delta: u64,
) -> Vectored<'control, 'table> {
    let table = table
        .subwindow(delta + capability.table_offset(), capability.table_bytes())
        .expect("the MSI-X table inside its BAR");
    let control = function
        .window()
        .subwindow(capability.offset(), capability.bytes())
        .expect("the MSI-X capability");
    // The line is homed on the core doing the routing, which is the core that
    // will run the driver: an interrupt landing anywhere else buys a message.
    let cpu = platform.cpu();
    let (token, message) = crate::pci::bind(platform, cpu).expect("one device interrupt line");
    let mut msix = MsiX::new(capability, control, table).expect("a complete MSI-X table");
    let vector = msix.route(0, message).expect("vector zero");
    msix.enable().expect("MSI-X enabled");
    Vectored { msix, token, vector }
}

/// A driver's end of a routed line.
pub(crate) struct Line {
    token: InterruptToken,
    ticks: u64,
}

impl Line {
    /// What the line has counted since it was last drained, without waiting.
    pub(crate) fn arrivals(&self) -> u64 {
        crate::pci::arrivals(self.token)
    }
}

impl Arrivals for Line {
    fn wait(&mut self) -> u64 {
        crate::pci::wait(self.token, self.ticks)
    }
}
