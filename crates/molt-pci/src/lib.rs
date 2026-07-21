//! PCI, expressed as windows rather than as a bus.
//!
//! There is no `pci` object here that owns the machine's devices, and no
//! function that takes a bus number and goes looking. Everything in this crate
//! works on an [`Mmio`](molt_arch::Mmio) window the platform already mapped, so
//! the authority to touch a function's configuration space *is* the window: a
//! caller who does not hold one cannot reach the bus, and one who holds a
//! function's window cannot reach its neighbour's.
//!
//! That shape falls out of the layering. [`ConfigSpace`] is what firmware said
//! — an ACPI `MCFG` allocation on x86_64, a `pci-host-ecam-generic` node on
//! RISC-V — and it says only where configuration space lives. Turning a
//! physical range into something the kernel may touch is
//! [`Inventory::device`](molt_arch::memory::Inventory::device) plus the
//! platform's [`DeviceMapper`](molt_arch::DeviceMapper), both of which already
//! refuse to hand out RAM as a device or to map a device window executable. By
//! the time this crate sees a window, those questions are settled.
//!
//! Enumeration is therefore [`Bus`]: hand it the window for one bus, get back
//! the functions that answered. Everything else — [`Bar`] sizing, the
//! capability list, [`MsiX`] — hangs off a [`Function`], and each borrows the
//! window it came from, so a BAR handle cannot outlive the mapping under it.
//!
//! # What this crate deliberately does not do
//!
//! It never computes an interrupt message. [`MsiX::route`] takes an
//! [`MsiMessage`](molt_arch::MsiMessage) the platform's
//! [`InterruptFabric`](molt_arch::InterruptFabric) produced, because the
//! address and data encode a destination this crate has no business knowing.
//!
//! It never enables bus mastering on its own. A device with
//! [`Command::BUS_MASTER`] set can write anywhere in physical memory, and until
//! the kernel programs an IOMMU that is a trust decision, not a driver
//! convenience. The caller has to ask for it in as many words — including when
//! it routes an MSI, which is a memory write from the device like any other and
//! is silently dropped without the bit.

#![no_std]

#[cfg(test)]
extern crate std;

mod bar;
mod function;
mod msi;

use molt_arch::pci::BUS_STRIDE;
use molt_arch::{ConfigSpace, Mmio, MmioError};

pub use crate::bar::{Bar, BarKind};
pub use crate::function::{Capabilities, Capability, Class, Command, Function};
pub use crate::msi::{MSI, MSIX, MsiCapability, MsiX, MsiXCapability, Vector, preferred};

/// Bytes of configuration space ECAM gives one function.
pub const FUNCTION_STRIDE: u64 = 4096;

/// Functions a device may implement, and devices a bus may carry.
const FUNCTIONS: u8 = 8;
const DEVICES: u8 = 32;

/// Why a configuration-space operation was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciError {
    /// The bus, device, or function number does not exist.
    Address,
    /// The access left the window it was made through.
    Window(MmioError),
    /// The capability list is malformed: a cycle, or a pointer out of range.
    ///
    /// Refused rather than followed, because a walk that trusts the device can
    /// be made to loop forever by a device that answers `0xff` to everything.
    Capability,
    /// The register does not describe what the caller asked it for: a BAR index
    /// this header type does not have, or a 64-bit BAR in the last slot.
    Layout,
    /// The device does not implement the capability the caller wanted.
    Absent,
    /// The vector is outside the table the device reported.
    Vector,
}

impl From<MmioError> for PciError {
    fn from(error: MmioError) -> Self {
        Self::Window(error)
    }
}

/// Where one function's configuration space lives, within one segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Address {
    bus: u8,
    device: u8,
    function: u8,
}

impl Address {
    /// Rejects a device or function number ECAM cannot encode.
    pub const fn new(bus: u8, device: u8, function: u8) -> Result<Self, PciError> {
        if device >= DEVICES || function >= FUNCTIONS {
            return Err(PciError::Address);
        }
        Ok(Self { bus, device, function })
    }

    pub const fn bus(self) -> u8 {
        self.bus
    }

    pub const fn device(self) -> u8 {
        self.device
    }

    pub const fn function(self) -> u8 {
        self.function
    }

    /// The offset of this function's 4 KiB window within its bus's window.
    pub const fn offset(self) -> u64 {
        (self.device as u64) << 15 | (self.function as u64) << 12
    }
}

/// The physical range one bus's configuration space occupies.
///
/// Buses are mapped one at a time rather than all at once: a whole segment is
/// 256 MiB of window for what is usually a handful of functions, and a mapping
/// that large is a large thing to get wrong.
pub fn bus_span(space: ConfigSpace, bus: u8) -> Result<molt_arch::memory::Span, PciError> {
    if bus < space.first_bus() || bus > space.last_bus() {
        return Err(PciError::Address);
    }
    let span = space.span().map_err(|_| PciError::Address)?;
    let start = span.start() + (bus - space.first_bus()) as u64 * BUS_STRIDE;
    molt_arch::memory::Span::new(start, start + BUS_STRIDE).map_err(|_| PciError::Address)
}

/// The functions that answer on one mapped bus.
///
/// Not an [`Iterator`]: each function borrows the bus window, and `Iterator`
/// cannot express an item that borrows the iterator.
pub struct Bus<'bus, 'window> {
    window: &'bus Mmio<'window>,
    number: u8,
    device: u8,
    function: u8,
    /// Whether the device currently being scanned answered as multi-function.
    /// Probing functions 1..8 of a single-function device is what makes some
    /// hardware alias them, so the header type gates the walk.
    multifunction: bool,
}

impl<'bus, 'window> Bus<'bus, 'window> {
    /// Scans bus `number`, whose configuration space `window` maps.
    pub const fn new(window: &'bus Mmio<'window>, number: u8) -> Self {
        Self { window, number, device: 0, function: 0, multifunction: false }
    }

    /// The next function present on the bus, or `None` at its end.
    pub fn function(&mut self) -> Option<Function<'bus>> {
        while self.device < DEVICES {
            let address = Address::new(self.number, self.device, self.function).ok()?;
            let found = self.probe(address);
            self.advance();
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn probe(&mut self, address: Address) -> Option<Function<'bus>> {
        let window = self.window.subwindow(address.offset(), FUNCTION_STRIDE).ok()?;
        let function = Function::probe(window, address).ok()??;
        if address.function() == 0 {
            self.multifunction = function.is_multifunction();
        }
        Some(function)
    }

    /// Steps to the next candidate, skipping functions 1..8 of a device whose
    /// function 0 is absent or single-function.
    fn advance(&mut self) {
        if self.multifunction && self.function + 1 < FUNCTIONS {
            self.function += 1;
        } else {
            self.device += 1;
            self.function = 0;
            self.multifunction = false;
        }
    }
}

#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use molt_arch::ConfigSpace;

    use super::{Address, Bus, PciError, bus_span};
    use crate::fake::Space;

    #[test]
    fn ecam_offset_matches_the_specified_encoding() {
        let address = Address::new(3, 31, 7).expect("a legal function number");

        assert_eq!(address.offset(), 31 << 15 | 7 << 12);
        assert_eq!(Address::new(0, 32, 0), Err(PciError::Address));
        assert_eq!(Address::new(0, 0, 8), Err(PciError::Address));
    }

    #[test]
    fn a_bus_window_covers_one_bus() {
        let space = ConfigSpace::new(0xb000_0000, 0, 0, 0xff).expect("firmware bus range");

        let span = bus_span(space, 2).expect("a bus inside the reported range");

        assert_eq!(span.start(), 0xb020_0000);
        assert_eq!(span.bytes(), 1 << 20);
    }

    #[test]
    fn a_bus_outside_the_reported_range_is_refused() {
        let space = ConfigSpace::new(0xb000_0000, 0, 0, 3).expect("firmware bus range");

        assert_eq!(bus_span(space, 4).err(), Some(PciError::Address));
    }

    #[test]
    fn scanning_finds_every_function_that_answers() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001);
        space.function(5, 0).header(0x1234, 0x0002);
        let window = space.window();

        let mut bus = Bus::new(&window, 0);

        let first = bus.function().expect("the function at 00:00.0");
        assert_eq!((first.vendor(), first.device()), (0x1234, 0x0001));
        let second = bus.function().expect("the function at 00:05.0");
        assert_eq!(second.address().device(), 5);
        assert!(bus.function().is_none(), "the scan invented a function");
    }

    #[test]
    fn a_single_function_device_is_probed_once() {
        let mut space = Space::new();
        // Function 0 is single-function, so 00:00.1 must never be read even
        // though this fixture answers there.
        space.function(0, 0).header(0x1234, 0x0001);
        space.function(0, 1).header(0x1234, 0x0002);
        let window = space.window();

        let mut bus = Bus::new(&window, 0);

        assert_eq!(bus.function().expect("00:00.0").device(), 0x0001);
        assert!(bus.function().is_none(), "a single-function device answered twice");
    }

    #[test]
    fn a_multifunction_device_reports_its_other_functions() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001).multifunction();
        space.function(0, 3).header(0x1234, 0x0004);
        let window = space.window();

        let mut bus = Bus::new(&window, 0);

        assert_eq!(bus.function().expect("00:00.0").device(), 0x0001);
        assert_eq!(bus.function().expect("00:00.3").device(), 0x0004);
        assert!(bus.function().is_none());
    }
}
