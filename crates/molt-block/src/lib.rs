//! Sector storage, described independently of the bus it hangs off.
//!
//! [`Device`] is the read contract; [`Writable`] adds sector writes and a
//! durability boundary. `molt-virtio` implements them over a virtqueue,
//! [`Loopback`] over bytes already in memory, and a future NVMe or SD driver
//! over whatever it likes — none of which the filesystem above has to know.

#![no_std]

#[cfg(test)]
extern crate std;

mod device;
mod fault;
mod loopback;

pub use crate::device::{Device, Disk, bounds};
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
    /// The device is deliberately read-only.
    ReadOnly,
    /// A fault-injection device cut power at this operation.
    PowerLoss,
}
