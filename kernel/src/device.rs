//! What bringing a PCI device up takes, before any driver sees it.
//!
//! PCI drivers share BAR mapping, MSI-X routing, and line release here.

use molt_arch::memory::{Inventory, Rights};
use molt_arch::{Mmio, Platform};
use molt_core::interrupt::InterruptToken;
use molt_pci::{Bar, Command, Function, MsiX, MsiXCapability, Vector};
use molt_virtio::{Location, Transport};

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

/// The BAR the MSI-X table lives in, mapped unless it is `mapped` already.
pub(crate) fn table_bar<P: Platform>(
    platform: &mut P,
    inventory: &Inventory<'_>,
    function: &mut Function<'_>,
    (mapped, at): (Bar, u8),
    index: u8,
) -> (Bar, Option<Mmio<'static>>) {
    if index == at {
        return (mapped, None);
    }
    let (bar, mapping) = map_bar(platform, inventory, function, index);
    (bar, Some(mapping))
}

/// Leaves `function` decoding memory with INTx and bus mastering off, and says
/// what was written.
///
/// The state a device stays in until its domain and its queue mappings exist:
/// nothing it could DMA into is mapped yet, so nothing it could DMA is allowed.
pub(crate) fn quiesce(function: &mut Function<'_>) -> Command {
    let quiesced = quiesced(function.command().expect("a readable command register"));
    function.set_command(quiesced).expect("a writable command register");
    quiesced
}

/// `command` with the function decoding memory and mastering nothing, which is
/// the one definition of quiesced there is.
pub(crate) const fn quiesced(command: Command) -> Command {
    command.with(Command::MEMORY).with(Command::INTX_DISABLE).without(Command::BUS_MASTER)
}

/// What a modern VirtIO function says about itself, and the one BAR it says it
/// in — structures spread over several would need several mappings, and no
/// device molt drives reports them that way.
pub(crate) fn transport(function: &Function<'_>) -> (Transport, u8) {
    let transport = Transport::probe(function).expect("a modern function describes its structures");
    let index = transport.common().bar();
    assert!(
        transport.notify().bar() == index && transport.device().bar() == index,
        "VirtIO structures split across BARs",
    );
    (transport, index)
}

/// The common, notify, and device-specific windows, cut out of the mapped BAR.
pub(crate) fn structures<'a>(
    registers: &'a Mmio<'_>,
    bar: Bar,
    transport: &Transport,
) -> (Mmio<'a>, Mmio<'a>, Mmio<'a>) {
    let delta = delta(bar);
    (
        subwindow(registers, delta, transport.common()),
        subwindow(registers, delta, transport.notify()),
        subwindow(registers, delta, transport.device()),
    )
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

impl molt_virtio::Arrivals for Line {
    fn wait(&mut self) -> u64 {
        crate::pci::wait(self.token, self.ticks)
    }
}

impl molt_nvme::Arrivals for Line {
    fn wait(&mut self) -> u64 {
        crate::pci::wait(self.token, self.ticks)
    }
}
