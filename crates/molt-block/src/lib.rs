//! Sector storage, described independently of the bus it hangs off.
//!
//! [`Device`] is the read contract a filesystem needs — how many sectors exist
//! and how to read some — and [`Write`] adds the durable side: sector writes and
//! the flush that orders them. `molt-virtio` implements both over a virtqueue,
//! [`Loopback`] implements them over bytes already in memory, and [`Fault`]
//! wraps a writable device in a volatile cache a test can cut power to.

#![no_std]

#[cfg(test)]
extern crate std;

mod device;
mod fault;
mod loopback;
mod write;

pub use crate::device::{Device, bounds};
pub use crate::fault::{Fault, Line};
pub use crate::loopback::Loopback;
pub use crate::write::Write;

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
