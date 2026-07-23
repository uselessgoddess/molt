//! A mapped device window and the checked accesses it permits.
//!
//! [`memory::Device`](crate::memory::Device) says a physical range *may* be a
//! device window. [`Mmio`] is what a platform hands back once it actually
//! mapped one: the only handle in the system that may touch those registers.
//!
//! Two rules make the handle worth having. Every access is bounds- and
//! alignment-checked against the window, so a driver that computes an offset
//! wrong gets an [`MmioError`] instead of writing into whatever the mapping
//! happens to neighbour. And a [`subwindow`](Mmio::subwindow) borrows its
//! parent, so a BAR carved out of a larger mapping cannot outlive it.
//!
//! The window is deliberately not `Sync`. Device registers are stateful and
//! order-sensitive; sharing one across cores is a decision a driver has to make
//! explicitly rather than something the type system grants by default.

use core::marker::PhantomData;

use crate::MappingError;
use crate::memory::{Device, Rights};

/// Why an access was refused before it reached the bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioError {
    /// The access, or the requested subwindow, leaves the mapped window.
    Range,
    /// The offset is not a multiple of the access width.
    Alignment,
}

/// A mapped device window.
///
/// Constructed by a [`DeviceMapper`]; every read and write is checked against
/// the window's length before it happens.
#[derive(Debug)]
pub struct Mmio<'window> {
    base: *mut u8,
    len: u64,
    /// Ties a [`subwindow`](Mmio::subwindow) to the mapping it was carved from.
    window: PhantomData<&'window mut [u8]>,
}

// SAFETY: an `Mmio` is a unique handle to a mapped range, so moving it between
// threads moves the whole claim. It is deliberately not `Sync`: two threads
// holding `&Mmio` could issue concurrent volatile writes to one register.
unsafe impl Send for Mmio<'_> {}

impl<'window> Mmio<'window> {
    /// Wraps a live device mapping of `len` bytes starting at `base`.
    ///
    /// # Safety
    ///
    /// `base` must be the virtual base of a mapping that covers `len` bytes,
    /// stays valid for `'window`, and was established through
    /// [`Device::mapping`] — that is, with device cacheability and without
    /// execute permission. The caller must not hand out a second window over
    /// any part of the same range.
    pub const unsafe fn new(base: *mut u8, len: u64) -> Self {
        Self { base, len, window: PhantomData }
    }

    /// The number of bytes the window covers.
    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Carves `len` bytes at `offset` out of this window.
    ///
    /// The result borrows `self`, so a BAR or an MSI-X table cannot outlive the
    /// mapping it lives in.
    pub fn subwindow(&self, offset: u64, len: u64) -> Result<Mmio<'_>, MmioError> {
        self.range(offset, len)?;
        // SAFETY: `range` proved `offset + len` stays inside this window, which
        // the constructor's contract already established as mapped. The
        // returned lifetime keeps the child within the parent's.
        Ok(unsafe { Mmio::new(self.base.add(offset as usize), len) })
    }

    pub fn read_u8(&self, offset: u64) -> Result<u8, MmioError> {
        let address = self.access(offset, 1)?;
        // SAFETY: `access` checked bounds and alignment, and the constructor
        // guarantees the window is mapped for the handle's lifetime.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn read_u16(&self, offset: u64) -> Result<u16, MmioError> {
        let address = self.access(offset, 2)?.cast::<u16>();
        // SAFETY: see `read_u8`; `access` also proved the offset is aligned.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn read_u32(&self, offset: u64) -> Result<u32, MmioError> {
        let address = self.access(offset, 4)?.cast::<u32>();
        // SAFETY: see `read_u8`; `access` also proved the offset is aligned.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn read_u64(&self, offset: u64) -> Result<u64, MmioError> {
        let address = self.access(offset, 8)?.cast::<u64>();
        // SAFETY: see `read_u8`; `access` also proved the offset is aligned.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn write_u8(&self, offset: u64, value: u8) -> Result<(), MmioError> {
        let address = self.access(offset, 1)?;
        // SAFETY: see `read_u8`. The window is not `Sync`, so no other thread
        // holds a reference through which to race this write.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    pub fn write_u16(&self, offset: u64, value: u16) -> Result<(), MmioError> {
        let address = self.access(offset, 2)?.cast::<u16>();
        // SAFETY: see `write_u8`.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    pub fn write_u32(&self, offset: u64, value: u32) -> Result<(), MmioError> {
        let address = self.access(offset, 4)?.cast::<u32>();
        // SAFETY: see `write_u8`.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    pub fn write_u64(&self, offset: u64, value: u64) -> Result<(), MmioError> {
        let address = self.access(offset, 8)?.cast::<u64>();
        // SAFETY: see `write_u8`.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    /// Checks one access of `width` bytes and returns the address it names.
    fn access(&self, offset: u64, width: u64) -> Result<*mut u8, MmioError> {
        if offset % width != 0 {
            return Err(MmioError::Alignment);
        }
        self.range(offset, width)?;
        // SAFETY: the offset was just proved to lie within the mapping.
        Ok(unsafe { self.base.add(offset as usize) })
    }

    /// Rejects a range that does not fit entirely inside the window.
    fn range(&self, offset: u64, len: u64) -> Result<(), MmioError> {
        match offset.checked_add(len) {
            Some(end) if end <= self.len => Ok(()),
            _ => Err(MmioError::Range),
        }
    }
}

/// Turns a typed device window into a mapping the kernel can touch.
///
/// Implemented per platform. The cache policy is not a parameter because
/// [`Device::mapping`] already decides it; a mapper that programmed anything
/// other than what it was told is what the live-table audit is for.
pub trait DeviceMapper {
    /// Maps `window` with `rights` and returns the handle to its registers.
    fn map_device(&mut self, window: Device, rights: Rights)
    -> Result<Mmio<'static>, MappingError>;
}

#[cfg(test)]
mod tests {
    use super::{Mmio, MmioError};

    fn window(bytes: &mut [u8]) -> Mmio<'_> {
        // SAFETY: the slice is live for the borrow, uniquely borrowed, and no
        // other window is handed out over it.
        unsafe { Mmio::new(bytes.as_mut_ptr(), bytes.len() as u64) }
    }

    #[test]
    fn write_reaches_the_register() {
        let mut registers = [0u8; 16];
        let mmio = window(&mut registers);

        mmio.write_u32(4, 0xdead_beef).expect("aligned write inside the window");

        assert_eq!(mmio.read_u32(4), Ok(0xdead_beef));
    }

    #[test]
    fn access_past_the_end_is_refused() {
        let mut registers = [0u8; 16];
        let mmio = window(&mut registers);

        assert_eq!(mmio.read_u32(16), Err(MmioError::Range));
        assert_eq!(mmio.read_u64(16), Err(MmioError::Range));
    }

    #[test]
    fn misaligned_access_is_refused() {
        let mut registers = [0u8; 16];
        let mmio = window(&mut registers);

        assert_eq!(mmio.read_u32(2), Err(MmioError::Alignment));
        assert_eq!(mmio.write_u16(1, 0), Err(MmioError::Alignment));
    }

    #[test]
    fn subwindow_rebases_offsets() {
        let mut registers = [0u8; 16];
        let mmio = window(&mut registers);
        let inner = mmio.subwindow(8, 8).expect("the upper half of the window");

        inner.write_u32(0, 0x0102_0304).expect("aligned write inside the subwindow");

        assert_eq!(mmio.read_u32(8), Ok(0x0102_0304));
        assert_eq!(inner.len(), 8);
        assert_eq!(inner.read_u32(8), Err(MmioError::Range));
    }

    #[test]
    fn subwindow_cannot_escape_its_parent() {
        let mut registers = [0u8; 16];
        let mmio = window(&mut registers);

        assert_eq!(mmio.subwindow(8, 9).err(), Some(MmioError::Range));
        assert_eq!(mmio.subwindow(u64::MAX, 1).err(), Some(MmioError::Range));
    }
}
