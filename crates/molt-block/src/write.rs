//! The write side of storage: durable sector writes and the flush that orders them.

use crate::{BlockError, Device};

/// Sector-addressed storage a filesystem writes through.
///
/// A write may be volatile: a device is free to hold it in a cache and lose it
/// to a power cut until [`flush`](Write::flush) returns. A checkpoint that must
/// survive a crash writes its blocks, flushes, and only then writes the
/// superblock that names them — the ordering the filesystem's two-copy
/// discipline rests on, and the reason `flush` promises order and not just
/// durability.
pub trait Write: Device {
    /// Writes `buf` to consecutive sectors starting at `sector`.
    ///
    /// Fails the same way [`Device::read`] does — [`BlockError::Unaligned`] for
    /// a partial sector, [`BlockError::Range`] past the end — out of the same
    /// [`bounds`](crate::bounds) check, and is all-or-nothing for the same
    /// reason.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError>;

    /// Makes every write issued before it durable before it returns.
    ///
    /// A device with no volatile cache satisfies this by doing nothing; one
    /// with a cache empties it. Order is the load-bearing half: a write issued
    /// after a returned flush never reaches the disk ahead of one issued before
    /// it.
    fn flush(&mut self) -> Result<(), BlockError>;
}

impl<D: Write + ?Sized> Write for &mut D {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        (**self).write(sector, buf)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        (**self).flush()
    }
}
