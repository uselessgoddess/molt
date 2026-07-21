//! The identity of one PCI function.

use core::fmt;

use crate::error::Error;

/// Devices a bus can carry.
pub const DEVICES: u8 = 32;
/// Functions a device can implement.
pub const FUNCTIONS: u8 = 8;
/// Bytes of configuration space one function owns under ECAM.
pub const WINDOW: usize = 4096;

/// A bus, device, and function number, within one segment.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Address {
    bus: u8,
    device: u8,
    function: u8,
}

impl Address {
    pub const fn new(bus: u8, device: u8, function: u8) -> Result<Self, Error> {
        if device >= DEVICES || function >= FUNCTIONS {
            return Err(Error::Address);
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

    /// Function zero of the same device, the only one a probe may assume
    /// exists: a device that answers nowhere else still answers here.
    pub const fn root(self) -> Self {
        Self { function: 0, ..self }
    }

    /// Byte offset of this function's configuration space within the ECAM
    /// region of its segment.
    pub const fn window(self) -> usize {
        (self.bus as usize) << 20 | (self.device as usize) << 15 | (self.function as usize) << 12
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}
