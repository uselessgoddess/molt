//! Message-signalled interrupts, in both shapes PCI defines.
//!
//! Neither type here decides what a message *is*. The address and the data are
//! produced by the platform's
//! [`InterruptFabric`](molt_arch::InterruptFabric), which is the only thing
//! that knows how a store turns into an interrupt on this machine, and are
//! written to the device verbatim. That is the whole reason
//! [`MsiMessage`](molt_arch::MsiMessage) exists as a type rather than as two
//! `u32`s: a driver cannot assemble one, so it cannot route a device at a
//! destination the kernel is not listening on.
//!
//! Both types also refuse to be the only thing standing between a device and
//! the legacy interrupt pin. Enabling either sets
//! [`Command::INTX_DISABLE`](crate::Command), because a device that can still
//! assert INTx has a second path to the CPU that nothing is waiting on.

use molt_arch::{Mmio, MsiMessage};

use crate::PciError;
use crate::function::{Capability, Command, Function};

/// The capability identifiers the specification assigns.
pub const MSI: u8 = 0x05;
pub const MSIX: u8 = 0x11;

/// Bytes one MSI-X table entry occupies.
const ENTRY: u64 = 16;

/// Offsets within an MSI-X table entry.
const ENTRY_ADDRESS_LOW: u64 = 0;
const ENTRY_ADDRESS_HIGH: u64 = 4;
const ENTRY_DATA: u64 = 8;
const ENTRY_CONTROL: u64 = 12;

/// Set in an entry's control word while the vector is masked.
const ENTRY_MASKED: u32 = 1 << 0;

/// One routed interrupt vector: the device's index for it, and the line the
/// platform will report it on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vector {
    index: u16,
}

impl Vector {
    pub const fn index(self) -> u16 {
        self.index
    }
}

/// What a function's MSI-X capability says about its table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsiXCapability {
    offset: u64,
    vectors: u16,
    table_bar: u8,
    table_offset: u64,
    pending_bar: u8,
    pending_offset: u64,
}

impl MsiXCapability {
    /// How many vectors the device implements.
    pub const fn vectors(self) -> u16 {
        self.vectors
    }

    /// Which BAR the vector table lives in, and where inside it.
    pub const fn table_bar(self) -> u8 {
        self.table_bar
    }

    pub const fn table_offset(self) -> u64 {
        self.table_offset
    }

    /// Bytes the vector table occupies.
    pub const fn table_bytes(self) -> u64 {
        self.vectors as u64 * ENTRY
    }

    pub const fn pending_bar(self) -> u8 {
        self.pending_bar
    }

    pub const fn pending_offset(self) -> u64 {
        self.pending_offset
    }

    /// Where the capability's own registers live in configuration space.
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Bytes of configuration space the capability occupies.
    pub const fn bytes(self) -> u64 {
        12
    }
}

/// What a function's MSI capability says about itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsiCapability {
    offset: u64,
    vectors: u16,
    wide: bool,
}

impl MsiCapability {
    /// The largest number of vectors the device can be given; always a power
    /// of two, and always allocated as a contiguous block if used at all.
    pub const fn vectors(self) -> u16 {
        self.vectors
    }

    /// Whether the device can be given a 64-bit message address.
    pub const fn is_wide(self) -> bool {
        self.wide
    }

    pub const fn offset(self) -> u64 {
        self.offset
    }
}

impl Function<'_> {
    /// Reads the MSI-X capability, if the function has one.
    pub fn msix(&self) -> Result<MsiXCapability, PciError> {
        let capability = self.capability(MSIX)?;
        let window = self.window();
        let control = window.read_u16(capability.offset() + 2)?;
        let table = window.read_u32(capability.offset() + 4)?;
        let pending = window.read_u32(capability.offset() + 8)?;
        Ok(MsiXCapability {
            offset: capability.offset(),
            // The field holds one less than the table size, so a device with a
            // single vector reports zero and there is no way to report none.
            vectors: (control & 0x7ff) + 1,
            table_bar: (table & 0b111) as u8,
            table_offset: (table & !0b111) as u64,
            pending_bar: (pending & 0b111) as u8,
            pending_offset: (pending & !0b111) as u64,
        })
    }

    /// Reads the MSI capability, if the function has one.
    pub fn msi(&self) -> Result<MsiCapability, PciError> {
        let capability = self.capability(MSI)?;
        let control = self.window().read_u16(capability.offset() + 2)?;
        Ok(MsiCapability {
            offset: capability.offset(),
            vectors: 1 << ((control >> 1) & 0b111),
            wide: control & (1 << 7) != 0,
        })
    }

    /// Routes the function's single MSI vector at `message` and enables it.
    ///
    /// Only one vector is requested. Multiple MSI vectors must be a contiguous
    /// power-of-two block whose low data bits the device varies itself, which
    /// is a worse fit for [`InterruptFabric`](molt_arch::InterruptFabric) than
    /// MSI-X's independent entries; a device that needs several should use
    /// MSI-X, and one that cannot gets one and says so.
    pub fn route_msi(
        &mut self,
        capability: MsiCapability,
        message: MsiMessage,
    ) -> Result<Vector, PciError> {
        let offset = capability.offset;
        let data = if capability.wide { offset + 0x0c } else { offset + 0x08 };
        if !capability.wide && message.address() > u32::MAX as u64 {
            return Err(PciError::Layout);
        }

        let window = self.window();
        window.write_u32(offset + 0x04, message.address() as u32)?;
        if capability.wide {
            window.write_u32(offset + 0x08, (message.address() >> 32) as u32)?;
        }
        window.write_u16(data, message.data() as u16)?;

        // Enable, with the multiple-message field left at zero: one vector.
        let control = window.read_u16(offset + 2)? & !(0b111 << 4);
        window.write_u16(offset + 2, control | 1)?;

        let command = self.command()?;
        self.set_command(command.with(Command::INTX_DISABLE))?;
        Ok(Vector { index: 0 })
    }

    /// Stops the function from raising MSI.
    pub fn silence_msi(&mut self, capability: MsiCapability) -> Result<(), PciError> {
        let control = self.window().read_u16(capability.offset + 2)?;
        self.window().write_u16(capability.offset + 2, control & !1)?;
        Ok(())
    }
}

/// A function's mapped MSI-X vector table.
///
/// `'control` borrows the configuration-space window and `'table` the BAR the
/// table lives in; they are separate mappings, so they are separate lifetimes.
pub struct MsiX<'control, 'table> {
    control: Mmio<'control>,
    table: Mmio<'table>,
    vectors: u16,
}

impl<'control, 'table> MsiX<'control, 'table> {
    /// Pairs a capability's registers with the table they describe.
    ///
    /// `table` must be exactly the window the capability points at; a table
    /// window shorter than the reported vector count is refused here rather
    /// than discovered by a write that lands past its end.
    pub fn new(
        capability: MsiXCapability,
        control: Mmio<'control>,
        table: Mmio<'table>,
    ) -> Result<Self, PciError> {
        if control.len() < capability.bytes() || table.len() < capability.table_bytes() {
            return Err(PciError::Vector);
        }
        Ok(Self { control, table, vectors: capability.vectors() })
    }

    pub const fn vectors(&self) -> u16 {
        self.vectors
    }

    /// Programs one vector with `message` and unmasks it.
    ///
    /// The entry is masked while it is written, because an entry updated in
    /// pieces can be sampled by a device mid-write and deliver a message
    /// assembled from two different routes.
    pub fn route(&mut self, index: u16, message: MsiMessage) -> Result<Vector, PciError> {
        let entry = self.entry(index)?;
        self.table.write_u32(entry + ENTRY_CONTROL, ENTRY_MASKED)?;
        self.table.write_u32(entry + ENTRY_ADDRESS_LOW, message.address() as u32)?;
        self.table.write_u32(entry + ENTRY_ADDRESS_HIGH, (message.address() >> 32) as u32)?;
        self.table.write_u32(entry + ENTRY_DATA, message.data())?;
        self.table.write_u32(entry + ENTRY_CONTROL, 0)?;
        Ok(Vector { index })
    }

    /// Masks one vector. The device keeps a pending bit rather than losing it.
    pub fn mask(&mut self, vector: Vector) -> Result<(), PciError> {
        let entry = self.entry(vector.index)?;
        self.table.write_u32(entry + ENTRY_CONTROL, ENTRY_MASKED)?;
        Ok(())
    }

    /// Turns the whole capability on, and INTx off with it.
    pub fn enable(&mut self, function: &mut Function<'_>) -> Result<(), PciError> {
        let control = self.control.read_u16(2)?;
        // Bit 15 enables, bit 14 masks every vector at once; clearing the
        // latter is what makes the per-entry masks the only thing in the way.
        self.control.write_u16(2, control & !(1 << 14) | 1 << 15)?;
        let command = function.command()?;
        function.set_command(command.with(Command::INTX_DISABLE))?;
        Ok(())
    }

    /// Turns the capability off without disturbing the routes in the table.
    pub fn disable(&mut self) -> Result<(), PciError> {
        let control = self.control.read_u16(2)?;
        self.control.write_u16(2, control & !(1 << 15))?;
        Ok(())
    }

    /// Whether the device is holding an undelivered message for `vector`.
    pub fn is_masked(&self, vector: Vector) -> Result<bool, PciError> {
        let entry = self.entry(vector.index)?;
        Ok(self.table.read_u32(entry + ENTRY_CONTROL)? & ENTRY_MASKED != 0)
    }

    fn entry(&self, index: u16) -> Result<u64, PciError> {
        if index >= self.vectors {
            return Err(PciError::Vector);
        }
        Ok(index as u64 * ENTRY)
    }
}

/// The capability a function implements, preferring the one with independent
/// per-vector routing.
pub fn preferred(function: &Function<'_>) -> Result<Capability, PciError> {
    function.capability(MSIX).or_else(|_| function.capability(MSI))
}

#[cfg(test)]
mod tests {
    use molt_arch::{Mmio, MsiMessage};

    use super::{MSI, MSIX, MsiX};
    use crate::fake::Space;
    use crate::function::{Command, Function};
    use crate::{Address, PciError};

    fn function(space: &mut Space) -> Function<'_> {
        Function::probe(space.config(0, 0), Address::new(0, 0, 0).expect("00:00.0"))
            .expect("a legal read")
            .expect("present")
    }

    fn table(bytes: &mut [u8]) -> Mmio<'_> {
        // SAFETY: the slice outlives the window and nothing else touches it.
        unsafe { Mmio::new(bytes.as_mut_ptr(), bytes.len() as u64) }
    }

    #[test]
    fn the_msix_capability_reports_its_table() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1af4, 0x1041).msix(0x40, 4, 1, 0x2000);
        let function = function(&mut space);

        let capability = function.msix().expect("an MSI-X capability");

        assert_eq!(capability.vectors(), 4);
        assert_eq!(capability.table_bar(), 1);
        assert_eq!(capability.table_offset(), 0x2000);
        assert_eq!(capability.table_bytes(), 64);
    }

    #[test]
    fn routing_writes_the_platform_message_verbatim() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1af4, 0x1041).msix(0x40, 2, 0, 0);
        let mut entries = [0u8; 32];
        let capability = function(&mut space).msix().expect("an MSI-X capability");
        let control = space.window();
        let control = control.subwindow(0x40, capability.bytes()).expect("the capability");
        let mut msix =
            MsiX::new(capability, control, table(&mut entries)).expect("a table large enough");

        let vector =
            msix.route(1, MsiMessage::new(0xfee0_0000, 0x0051)).expect("a vector the device has");

        assert_eq!(vector.index(), 1);
        assert_eq!(msix.is_masked(vector), Ok(false));
        assert_eq!(&entries[16..28], &[0x00, 0x00, 0xe0, 0xfe, 0, 0, 0, 0, 0x51, 0, 0, 0]);
    }

    #[test]
    fn a_vector_the_device_does_not_have_is_refused() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1af4, 0x1041).msix(0x40, 2, 0, 0);
        let mut entries = [0u8; 32];
        let capability = function(&mut space).msix().expect("an MSI-X capability");
        let control = space.window();
        let control = control.subwindow(0x40, capability.bytes()).expect("the capability");
        let mut msix =
            MsiX::new(capability, control, table(&mut entries)).expect("a table large enough");

        assert_eq!(msix.route(2, MsiMessage::new(0xfee0_0000, 0x51)), Err(PciError::Vector));
    }

    #[test]
    fn a_table_window_too_small_is_refused() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1af4, 0x1041).msix(0x40, 4, 0, 0);
        let mut entries = [0u8; 32];
        let capability = function(&mut space).msix().expect("an MSI-X capability");
        let control = space.window();
        let control = control.subwindow(0x40, capability.bytes()).expect("the capability");

        assert!(MsiX::new(capability, control, table(&mut entries)).is_err());
    }

    #[test]
    fn msi_is_programmed_with_the_message_and_enabled() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x11e8).msi(0x40, true);
        let mut function = function(&mut space);
        let capability = function.msi().expect("an MSI capability");

        function
            .route_msi(capability, MsiMessage::new(0xfee0_0000, 0x0052))
            .expect("a device with one vector");

        let window = function.window();
        assert_eq!(window.read_u32(0x44), Ok(0xfee0_0000));
        assert_eq!(window.read_u32(0x48), Ok(0));
        assert_eq!(window.read_u16(0x4c), Ok(0x0052));
        assert_eq!(window.read_u16(0x42).map(|c| c & 1), Ok(1), "MSI was left disabled");
        assert!(function.command().expect("readable").contains(Command::INTX_DISABLE));
    }

    #[test]
    fn a_narrow_device_refuses_a_wide_message() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x11e8).msi(0x40, false);
        let mut function = function(&mut space);
        let capability = function.msi().expect("an MSI capability");

        let routed = function.route_msi(capability, MsiMessage::new(0x1_0000_0000, 0x52));

        assert_eq!(routed, Err(PciError::Layout));
    }

    #[test]
    fn msix_is_preferred_over_msi() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1af4, 0x1041).msi(0x40, true).msix(0x60, 1, 0, 0);
        let function = function(&mut space);

        assert_eq!(super::preferred(&function).expect("a capability").id(), MSIX);
        assert_eq!(function.capability(MSI).expect("an MSI capability").offset(), 0x40);
    }
}
