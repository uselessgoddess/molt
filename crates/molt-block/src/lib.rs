//! Sector storage, described independently of the bus it hangs off.
//!
//! [`Device`] is the whole contract a read-only filesystem needs: how many
//! sectors exist and how to read some of them. A writer asks for one thing
//! more, [`Disk`], which adds writes and a flush to order them. `molt-virtio`
//! implements them over a virtqueue, [`Loopback`] over bytes already in memory,
//! and a future NVMe or SD driver over whatever it likes — none of which the
//! filesystem above has to know. [`Fault`] wraps any of them to cut the power
//! mid-write and prove the layer above survives it.

#![no_std]

#[cfg(test)]
extern crate std;

mod device;
mod disk;
mod fault;
mod loopback;

pub use crate::device::{Device, bounds};
pub use crate::disk::Disk;
pub use crate::fault::Fault;
pub use crate::loopback::Loopback;

/// A sector is 512 bytes, the unit every device address is counted in.
pub const SECTOR: usize = 512;

/// Why a device refused a read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockError {
    /// The request would leave the end of the device.
    Range,
    /// The buffer is not a whole number of sectors.
    Unaligned,
    /// The device reported a failure, or is not one this driver can drive.
    Device,
    /// The device did not answer within the driver's budget.
    Timeout,
}
