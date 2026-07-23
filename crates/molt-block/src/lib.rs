//! Sector storage, described independently of the bus it hangs off.
//!
//! [`Device`] is the whole contract a filesystem needs: how many sectors exist
//! and how to read some of them. `molt-virtio` implements it over a virtqueue,
//! [`Loopback`] implements it over bytes already in memory, and a future NVMe or
//! SD driver implements it over whatever it likes — none of which the
//! filesystem above has to know.

#![no_std]

#[cfg(test)]
extern crate std;

mod device;
mod loopback;

pub use crate::device::{Device, bounds};
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
