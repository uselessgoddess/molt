//! A modern VirtIO block driver, built out of the frames the kernel owns.
//!
//! The pieces mirror the transport the specification defines. [`Transport`]
//! finds the device's structures in its PCI BARs; [`Common`] drives the
//! initialization handshake and programs a queue; [`Queue`] is one split
//! virtqueue laid over [`Region`](molt_arch::dma::Region)s the device reads and
//! writes; [`Notify`] kicks the device; and [`Block`] ties them together to
//! read and write sectors, flush them durably, and, on [`reset`](Block::reset),
//! reclaim every frame only after the device has been told to stop.
//!
//! Above the driver there are only [`molt_block::Device`] and
//! [`molt_block::Writable`], which [`Block`] implements: sectors in, sectors
//! out, with the virtqueue invisible.

#![no_std]

#[cfg(test)]
extern crate std;

mod block;
mod config;
mod notify;
mod queue;
mod request;
mod transport;

use molt_arch::MmioError;
use molt_arch::dma::DmaError;
use molt_block::BlockError;
use molt_pci::PciError;

pub use crate::block::Block;
pub use crate::config::Common;
pub use crate::notify::Notify;
pub use crate::queue::{Queue, Segment, Used};
pub use crate::transport::{Location, Structure, Transport};

/// Why a VirtIO request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioError {
    /// A structure the driver needs is not in the device's capabilities, or the
    /// queue the driver asked for does not exist.
    Missing,
    /// A configuration-space read while probing the transport failed.
    Pci(PciError),
    /// A register access left its window.
    Mmio(MmioError),
    /// A DMA region access left its bounds.
    Dma(DmaError),
    /// The device would not accept the features the driver requires.
    Features,
    /// The device advertises itself as read-only.
    ReadOnly,
    /// The device reported a size or layout the driver cannot honour.
    Device,
    /// The submission ring is full; the caller must drain completions first.
    Full,
    /// A request did not complete within the driver's spin budget.
    Timeout,
}

impl From<PciError> for VirtioError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

impl From<MmioError> for VirtioError {
    fn from(error: MmioError) -> Self {
        Self::Mmio(error)
    }
}

impl From<DmaError> for VirtioError {
    fn from(error: DmaError) -> Self {
        Self::Dma(error)
    }
}

impl From<VirtioError> for BlockError {
    fn from(error: VirtioError) -> Self {
        match error {
            VirtioError::Timeout => Self::Timeout,
            VirtioError::Dma(DmaError::Range) => Self::Range,
            VirtioError::ReadOnly => Self::ReadOnly,
            _ => Self::Device,
        }
    }
}
