//! A configuration space made of ordinary memory, for host tests.
//!
//! Real configuration space answers `0xffff` for a function that is not there,
//! so the buffer starts filled with ones and a builder carves present
//! functions out of it. What this cannot model is a register whose read value
//! depends on what was written to it — BAR sizing, most of all — which is why
//! [`decode`](crate::bar) is a pure function tested on its own.

extern crate std;

use std::vec;
use std::vec::Vec;

use molt_arch::Mmio;
use molt_arch::pci::BUS_STRIDE;

use crate::FUNCTION_STRIDE;

/// One bus's worth of configuration space.
pub struct Space {
    bytes: Vec<u8>,
}

impl Space {
    /// A bus on which nothing answers.
    pub fn new() -> Self {
        Self { bytes: vec![0xff; BUS_STRIDE as usize] }
    }

    /// Starts describing the function at `device.function`.
    pub fn function(&mut self, device: u8, function: u8) -> Builder<'_> {
        let base = Self::base(device, function);
        self.bytes[base..base + FUNCTION_STRIDE as usize].fill(0);
        Builder { bytes: &mut self.bytes[base..base + FUNCTION_STRIDE as usize], last: None }
    }

    /// The window a bus scan would be given.
    pub fn window(&mut self) -> Mmio<'_> {
        // SAFETY: the buffer outlives the borrow, and the mutable borrow means
        // no other window over it exists.
        unsafe { Mmio::new(self.bytes.as_mut_ptr(), self.bytes.len() as u64) }
    }

    /// The window one function's configuration space would be given.
    pub fn config(&mut self, device: u8, function: u8) -> Mmio<'_> {
        let base = Self::base(device, function);
        // SAFETY: `base + FUNCTION_STRIDE` is inside the buffer by
        // construction, which outlives the borrow.
        unsafe { Mmio::new(self.bytes.as_mut_ptr().add(base), FUNCTION_STRIDE) }
    }

    fn base(device: u8, function: u8) -> usize {
        (device as usize) << 15 | (function as usize) << 12
    }
}

/// Fills in one function's registers.
pub struct Builder<'space> {
    bytes: &'space mut [u8],
    /// The last capability added, so the next one can be chained onto it.
    last: Option<u64>,
}

impl Builder<'_> {
    pub fn header(&mut self, vendor: u16, device: u16) -> &mut Self {
        self.write_u16(0x00, vendor);
        self.write_u16(0x02, device)
    }

    pub fn class(&mut self, class: u8, subclass: u8, interface: u8) -> &mut Self {
        self.write_u8(0x09, interface);
        self.write_u8(0x0a, subclass);
        self.write_u8(0x0b, class)
    }

    pub fn multifunction(&mut self) -> &mut Self {
        let header = self.bytes[0x0e];
        self.write_u8(0x0e, header | 1 << 7)
    }

    pub fn register(&mut self, offset: u64, value: u32) -> &mut Self {
        self.write_u32(offset, value)
    }

    /// Adds a capability with an explicit `next` pointer, so a test can build
    /// a list no sane device would.
    pub fn capability(&mut self, offset: u64, id: u8, next: u8) -> &mut Self {
        self.link(offset, id);
        self.write_u8(offset + 1, next)
    }

    /// Adds an MSI capability, `wide` when it takes a 64-bit address.
    pub fn msi(&mut self, offset: u64, wide: bool) -> &mut Self {
        self.link(offset, crate::msi::MSI);
        self.write_u16(offset + 2, if wide { 1 << 7 } else { 0 })
    }

    /// Adds an MSI-X capability for `vectors` entries at `table` in BAR `bar`.
    pub fn msix(&mut self, offset: u64, vectors: u16, bar: u8, table: u32) -> &mut Self {
        self.link(offset, crate::msi::MSIX);
        self.write_u16(offset + 2, vectors - 1);
        self.write_u32(offset + 4, table | bar as u32);
        self.write_u32(offset + 8, table | bar as u32)
    }

    /// Chains a capability onto the list, starting it if it is the first.
    fn link(&mut self, offset: u64, id: u8) {
        match self.last {
            Some(previous) => self.write_u8(previous + 1, offset as u8),
            None => {
                self.write_u16(0x06, 1 << 4);
                self.write_u8(0x34, offset as u8)
            }
        };
        self.write_u8(offset, id);
        self.write_u8(offset + 1, 0);
        self.last = Some(offset);
    }

    fn write_u8(&mut self, offset: u64, value: u8) -> &mut Self {
        self.bytes[offset as usize] = value;
        self
    }

    fn write_u16(&mut self, offset: u64, value: u16) -> &mut Self {
        self.bytes[offset as usize..offset as usize + 2].copy_from_slice(&value.to_le_bytes());
        self
    }

    fn write_u32(&mut self, offset: u64, value: u32) -> &mut Self {
        self.bytes[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
        self
    }
}
