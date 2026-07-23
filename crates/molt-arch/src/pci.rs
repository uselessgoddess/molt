//! What firmware says about PCI, and nothing about what PCI means.
//!
//! Finding the enhanced configuration access mechanism is a firmware question
//! with a different answer per platform: an ACPI `MCFG` table on x86_64, a
//! `pci-host-ecam-generic` node in the device tree on RISC-V.
//!
//! *interprets*, configuration space, headers, BARs, capabilities, MSI-X — is
//! the `molt-pci` crate.

use crate::memory::{Error, Span};

/// A 4 KiB configuration region per function, for one segment's bus range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigSpace {
    base: u64,
    segment: u16,
    first_bus: u8,
    last_bus: u8,
}

/// Bytes ECAM gives every function: 4 KiB, 8 functions, 32 devices per bus.
pub const BUS_STRIDE: u64 = 32 * 8 * 4096;

impl ConfigSpace {
    /// Rejects a bus range firmware reported backwards.
    pub const fn new(base: u64, segment: u16, first_bus: u8, last_bus: u8) -> Result<Self, Error> {
        if first_bus > last_bus {
            return Err(Error::Range);
        }
        Ok(Self { base, segment, first_bus, last_bus })
    }

    pub const fn segment(self) -> u16 {
        self.segment
    }

    pub const fn first_bus(self) -> u8 {
        self.first_bus
    }

    pub const fn last_bus(self) -> u8 {
        self.last_bus
    }

    /// The physical range the whole bus number range occupies.
    pub const fn span(self) -> Result<Span, Error> {
        let buses = (self.last_bus - self.first_bus) as u64 + 1;
        Span::new(self.base, self.base + buses * BUS_STRIDE)
    }
}

#[cfg(test)]
mod tests {
    use super::{BUS_STRIDE, ConfigSpace};
    use crate::memory::Error;

    #[test]
    fn span_covers_every_reported_bus() {
        let space = ConfigSpace::new(0xb000_0000, 0, 0, 0xff).expect("firmware bus range");

        let span = space.span().expect("aligned ECAM span");

        assert_eq!(span.start(), 0xb000_0000);
        assert_eq!(span.bytes(), 256 * BUS_STRIDE);
    }

    #[test]
    fn backwards_bus_range_is_refused() {
        assert_eq!(ConfigSpace::new(0xb000_0000, 0, 4, 1), Err(Error::Range));
    }
}
