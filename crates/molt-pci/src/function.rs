//! One function's configuration space, and what may be read out of it.

use molt_arch::Mmio;

use crate::{Address, PciError};

/// Header offsets this module reads. Named because `0x34` is not self-evident
/// and the specification's numbers are the only names these registers have.
mod register {
    pub const VENDOR: u64 = 0x00;
    pub const DEVICE: u64 = 0x02;
    pub const COMMAND: u64 = 0x04;
    pub const STATUS: u64 = 0x06;
    pub const INTERFACE: u64 = 0x09;
    pub const SUBCLASS: u64 = 0x0a;
    pub const CLASS: u64 = 0x0b;
    pub const HEADER_TYPE: u64 = 0x0e;
    pub const CAPABILITIES: u64 = 0x34;
}

/// The vendor identifier a bus reports for a function that is not there.
const ABSENT: u16 = 0xffff;

/// Set in the status register when the function has a capability list.
const STATUS_CAPABILITIES: u16 = 1 << 4;

/// Set in the header type when the device implements more than one function.
const HEADER_MULTIFUNCTION: u8 = 1 << 7;

/// What a function will respond to, as the command register encodes it.
///
/// A bit field rather than a set of booleans because the register has to be
/// written back whole: read-modify-write on a device register is where two
/// independent "just enable my one bit" helpers quietly undo each other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Command(u16);

impl Command {
    /// Respond to port-I/O accesses.
    pub const IO: Self = Self(1 << 0);
    /// Respond to memory accesses, which is what makes a BAR decode.
    pub const MEMORY: Self = Self(1 << 1);
    /// Initiate transactions of its own — that is, read and write host memory.
    pub const BUS_MASTER: Self = Self(1 << 2);
    /// Suppress the legacy pin-based interrupt.
    pub const INTX_DISABLE: Self = Self(1 << 10);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// What a function says it is, in the specification's three-level taxonomy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Class {
    class: u8,
    subclass: u8,
    interface: u8,
}

impl Class {
    pub const fn class(self) -> u8 {
        self.class
    }

    pub const fn subclass(self) -> u8 {
        self.subclass
    }

    pub const fn interface(self) -> u8 {
        self.interface
    }
}

/// A present PCI function, reachable only through the window that maps it.
#[derive(Debug)]
pub struct Function<'w> {
    window: Mmio<'w>,
    address: Address,
}

impl<'w> Function<'w> {
    /// Reads the vendor register to decide whether anything is there.
    ///
    /// Returns `Ok(None)` for an absent function, which is the ordinary answer
    /// for most of a bus and not an error.
    pub fn probe(window: Mmio<'w>, address: Address) -> Result<Option<Self>, PciError> {
        let vendor = window.read_u16(register::VENDOR)?;
        if vendor == ABSENT {
            return Ok(None);
        }
        Ok(Some(Self { window, address }))
    }

    pub const fn address(&self) -> Address {
        self.address
    }

    /// The window this function's registers live in.
    pub const fn window(&self) -> &Mmio<'w> {
        &self.window
    }

    pub fn vendor(&self) -> u16 {
        self.window.read_u16(register::VENDOR).unwrap_or(ABSENT)
    }

    pub fn device(&self) -> u16 {
        self.window.read_u16(register::DEVICE).unwrap_or(ABSENT)
    }

    pub fn class(&self) -> Class {
        Class {
            class: self.window.read_u8(register::CLASS).unwrap_or(0),
            subclass: self.window.read_u8(register::SUBCLASS).unwrap_or(0),
            interface: self.window.read_u8(register::INTERFACE).unwrap_or(0),
        }
    }

    /// The header layout: 0 for a device, 1 for a bridge.
    pub fn header_type(&self) -> u8 {
        self.window.read_u8(register::HEADER_TYPE).unwrap_or(0) & !HEADER_MULTIFUNCTION
    }

    pub fn is_multifunction(&self) -> bool {
        self.window.read_u8(register::HEADER_TYPE).unwrap_or(0) & HEADER_MULTIFUNCTION != 0
    }

    pub fn command(&self) -> Result<Command, PciError> {
        Ok(Command(self.window.read_u16(register::COMMAND)?))
    }

    /// Writes the command register whole.
    pub fn set_command(&mut self, command: Command) -> Result<(), PciError> {
        self.window.write_u16(register::COMMAND, command.bits())?;
        Ok(())
    }

    /// The function's capabilities, or an empty walk if it has none.
    pub fn capabilities(&self) -> Result<Capabilities<'_, 'w>, PciError> {
        let status = self.window.read_u16(register::STATUS)?;
        let next = if status & STATUS_CAPABILITIES == 0 {
            0
        } else {
            self.window.read_u8(register::CAPABILITIES)?
        };
        Ok(Capabilities { function: self, next, seen: 0 })
    }

    /// The first capability with `id`, if the function has one.
    pub fn capability(&self, id: u8) -> Result<Capability, PciError> {
        let mut capabilities = self.capabilities()?;
        for capability in capabilities.by_ref() {
            let capability = capability?;
            if capability.id() == id {
                return Ok(capability);
            }
        }
        Err(PciError::Absent)
    }
}

/// A capability's identifier and where its registers start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capability {
    id: u8,
    offset: u64,
}

impl Capability {
    pub const fn id(self) -> u8 {
        self.id
    }

    /// The offset of the capability's first register in configuration space.
    pub const fn offset(self) -> u64 {
        self.offset
    }
}

/// Walks a function's capability list.
///
/// The walk is bounded and refuses to revisit: the list is a linked list the
/// *device* supplies, and a device that answers `0xff` to every read describes
/// a cycle. Yielding [`PciError::Capability`] rather than stopping quietly
/// keeps a broken device from looking like a device without capabilities.
pub struct Capabilities<'function, 'window> {
    function: &'function Function<'window>,
    next: u8,
    seen: u32,
}

/// The header is 64 bytes, so no capability can start below it, and a list
/// cannot be longer than the 192 bytes that remain at 4 bytes each.
const CAPABILITY_FLOOR: u8 = 0x40;
const CAPABILITY_LIMIT: u32 = 48;

impl Iterator for Capabilities<'_, '_> {
    type Item = Result<Capability, PciError>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next & !0b11;
        if offset == 0 {
            return None;
        }
        if offset < CAPABILITY_FLOOR || self.seen >= CAPABILITY_LIMIT {
            self.next = 0;
            return Some(Err(PciError::Capability));
        }
        self.seen += 1;
        let window = self.function.window();
        let id = match window.read_u8(offset as u64) {
            Ok(id) => id,
            Err(error) => {
                self.next = 0;
                return Some(Err(error.into()));
            }
        };
        match window.read_u8(offset as u64 + 1) {
            Ok(next) => self.next = next,
            Err(error) => {
                self.next = 0;
                return Some(Err(error.into()));
            }
        }
        Some(Ok(Capability { id, offset: offset as u64 }))
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Function};
    use crate::fake::Space;
    use crate::{Address, PciError};

    fn address() -> Address {
        Address::new(0, 0, 0).expect("00:00.0")
    }

    #[test]
    fn absent_function_is_not_error() {
        let mut space = Space::new();

        let function = Function::probe(space.config(0, 0), address()).expect("a legal read");

        assert!(function.is_none(), "an unanswered bus invented a device");
    }

    #[test]
    fn header_reports_device_writes() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1af4, 0x1000).class(0x02, 0x00, 0x00);
        let window = space.window();
        let config = window.subwindow(0, 4096).expect("the first function's window");

        let function = Function::probe(config, address()).expect("a legal read").expect("present");

        assert_eq!((function.vendor(), function.device()), (0x1af4, 0x1000));
        assert_eq!(function.class().class(), 0x02);
        assert_eq!(function.header_type(), 0);
    }

    #[test]
    fn command_register_written_whole() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001);
        let mut function =
            Function::probe(space.config(0, 0), address()).expect("a legal read").expect("present");

        let wanted = Command::MEMORY.with(Command::INTX_DISABLE);
        function.set_command(wanted).expect("a legal write");

        assert_eq!(function.command(), Ok(wanted));
        assert!(function.command().expect("readable").contains(Command::MEMORY));
        assert!(!function.command().expect("readable").contains(Command::BUS_MASTER));
    }

    #[test]
    fn capabilities_walked_in_order() {
        let mut space = Space::new();
        space
            .function(0, 0)
            .header(0x1234, 0x0001)
            .capability(0x40, 0x05, 0x50)
            .capability(0x50, 0x11, 0x00);
        let function =
            Function::probe(space.config(0, 0), address()).expect("a legal read").expect("present");

        let mut found = [0u8; 4];
        let mut len = 0;
        for capability in function.capabilities().expect("a capability list") {
            found[len] = capability.expect("a well-formed capability").id();
            len += 1;
        }

        assert_eq!(&found[..len], &[0x05, 0x11]);
        assert_eq!(function.capability(0x11).expect("MSI-X").offset(), 0x50);
        assert_eq!(function.capability(0x10), Err(PciError::Absent));
    }

    #[test]
    fn function_without_capabilities_reports_none() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001);
        let function =
            Function::probe(space.config(0, 0), address()).expect("a legal read").expect("present");

        assert_eq!(function.capabilities().expect("an empty walk").count(), 0);
        assert_eq!(function.capability(0x11), Err(PciError::Absent));
    }

    #[test]
    fn looping_capability_list_refused() {
        let mut space = Space::new();
        // Two capabilities that point at each other: a device can describe this
        // and a walk that trusts it never returns.
        space
            .function(0, 0)
            .header(0x1234, 0x0001)
            .capability(0x40, 0x05, 0x50)
            .capability(0x50, 0x11, 0x40);
        let function =
            Function::probe(space.config(0, 0), address()).expect("a legal read").expect("present");

        let refused = function
            .capabilities()
            .expect("a capability list")
            .any(|capability| capability == Err(PciError::Capability));

        assert!(refused, "the walk followed a cycle instead of refusing it");
    }

    #[test]
    fn capability_inside_header_refused() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001).capability(0x40, 0x05, 0x10);
        let function =
            Function::probe(space.config(0, 0), address()).expect("a legal read").expect("present");

        let walked = function.capabilities().expect("a capability list").last();

        assert_eq!(walked, Some(Err(PciError::Capability)));
    }
}
