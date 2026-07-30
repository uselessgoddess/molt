//! Sector storage, described independently of the bus it hangs off.
//!
//! [`Device`] is the read contract; [`Disk`] adds sector writes and a
//! durability boundary. `molt-virtio` implements them over a virtqueue,
//! [`Loopback`] over bytes already in memory, and a future NVMe or SD driver
//! over whatever it likes — none of which the filesystem above has to know.
//!
//! Both traits block. [`channel`] puts a ring in front of one of them so the
//! filesystem submits and awaits instead, and only [`Backing`] still calls a
//! device directly.

#![no_std]
#![feature(allocator_api)]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod device;
mod fault;
mod loopback;
mod ring;

pub use crate::device::{Device, Disk, bounds};
pub use crate::fault::Fault;
pub use crate::loopback::Loopback;
pub use crate::ring::{
    BLOCK, Backing, BlockClient, BlockDone, BlockDriver, BlockOp, Buffer, channel,
};

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
