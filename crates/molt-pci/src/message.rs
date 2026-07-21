//! What a device writes to raise an interrupt.

/// The address and payload of one message signalled interrupt.
///
/// An MSI is a posted memory write and nothing more: the interrupt controller
/// decodes `address` and `data` into a vector. Both values come from the
/// platform, never from the device or the driver, which is why this type is
/// opaque here — the PCI side only copies it into a table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Message {
    address: u64,
    data: u32,
}

impl Message {
    pub const fn new(address: u64, data: u32) -> Self {
        Self { address, data }
    }

    pub const fn address(self) -> u64 {
        self.address
    }

    pub const fn data(self) -> u32 {
        self.data
    }
}
