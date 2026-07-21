//! The capability list, walked without trusting it.

use crate::config::{self, Config};
use crate::function::Function;

/// Message signalled interrupts, the form without a table.
pub const MSI: u8 = 0x05;
/// A vendor's own structure, which VirtIO uses to describe its regions.
pub const VENDOR: u8 = 0x09;
/// PCI Express.
pub const EXPRESS: u8 = 0x10;
/// Message signalled interrupts with a table in a BAR.
pub const MSIX: u8 = 0x11;

/// The first offset a capability may live at: everything below is the header.
const FIRST: u16 = 0x40;
/// The last offset a capability header fits at.
const LAST: u16 = 0xfc;
/// A capability header is four bytes, so this many cannot fit in the space
/// between [`FIRST`] and [`LAST`]. Reaching it means the list points at itself.
const LIMIT: usize = (LAST - FIRST) as usize / 4 + 1;

/// One entry of a function's capability list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capability {
    id: u8,
    offset: u16,
}

impl Capability {
    pub const fn id(self) -> u8 {
        self.id
    }

    /// Where the capability's registers start in configuration space.
    pub const fn offset(self) -> u16 {
        self.offset
    }
}

/// A function's capability list, in the order the device chains it.
///
/// The chain is device-supplied data, so it is walked defensively: an offset
/// outside the header space ends the walk, and so does one more step than the
/// space can hold. A malformed list yields fewer capabilities; it never spins
/// and never reads outside the window.
pub struct Capabilities<'c, C: Config + ?Sized> {
    function: Function<'c, C>,
    next: u16,
    steps: usize,
}

impl<'c, C: Config + ?Sized> Capabilities<'c, C> {
    pub(crate) fn new(function: Function<'c, C>) -> Self {
        let present =
            function.config().read(function.address(), config::COMMAND) & config::HAS_CAPABILITIES;
        let next = match present {
            0 => 0,
            _ => u16::from(config::read8(
                function.config(),
                function.address(),
                config::CAPABILITIES,
            )),
        };
        Self { function, next, steps: 0 }
    }
}

impl<C: Config + ?Sized> Iterator for Capabilities<'_, C> {
    type Item = Capability;

    fn next(&mut self) -> Option<Capability> {
        let offset = self.next & !3;
        if !(FIRST..=LAST).contains(&offset) || self.steps == LIMIT {
            return None;
        }
        self.steps += 1;
        let header = self.function.config().read(self.function.address(), offset);
        self.next = (header >> 8) as u16 & 0xff;
        Some(Capability { id: header as u8, offset })
    }
}
