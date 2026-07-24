//! The write side of storage, opted into separately from reading.
//!
//! A filesystem that only reads asks for a [`Device`]; one that checkpoints
//! asks for a [`Disk`]. Splitting the two keeps a read-only image, a signed
//! artifact, or a worn-out medium from having to pretend it can take a write —
//! the type says which it is, and a loopback or virtio driver adds `Disk` only
//! when it means it.

use crate::{BlockError, Device};

/// Sector-addressed storage a filesystem writes through.
///
/// A write is all-or-nothing, like a read, and [`flush`](Disk::flush) is the
/// only ordering the layer above gets: bytes written before a flush that
/// returns are durable, bytes after it may not be. A checkpoint leans on
/// exactly that — regions, flush, then the superblock that points at them.
pub trait Disk: Device {
    /// Writes `buf` over consecutive sectors starting at `sector`.
    ///
    /// Fails with [`BlockError::Unaligned`] unless `buf` is a whole number of
    /// sectors, and with [`BlockError::Range`] if the write would leave the
    /// device. Implementors get both checks from [`bounds`](crate::bounds).
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError>;

    /// Makes every write that returned before this call durable.
    ///
    /// Returns only once the device has committed them, so the caller may
    /// order a later write after this one and rely on the earlier batch
    /// surviving a power loss between the two.
    fn flush(&mut self) -> Result<(), BlockError>;
}

impl<D: Disk + ?Sized> Disk for &mut D {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        (**self).write(sector, buf)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        (**self).flush()
    }
}
