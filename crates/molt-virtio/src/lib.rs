//! A modern VirtIO block driver, built out of the frames the kernel owns.
//!
//! The pieces mirror the transport the specification defines. [`Transport`]
//! finds the device's structures in its PCI BARs; [`Common`] drives the
//! initialization handshake and programs a queue; [`Queue`] is one split
//! virtqueue laid over [`Region`](molt_arch::dma::Region)s the device reads and
//! writes; [`Notify`] kicks the device; and [`Block`] ties them together to
//! read sectors and, on [`reset`](Block::reset), to reclaim every frame only
//! after the device has been told to stop.
//!
//! Above the driver there is only [`molt_block::Device`], which [`Block`]
//! implements: sectors in, sectors out, with the virtqueue invisible.
//!
//! The write path is deliberately absent. Stage 2.4's filesystem is read-only,
//! so the driver never issues a flush and never hands the device a writable
//! sector — the one operation is a read.

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

/// Collapses a driver failure into what a filesystem can act on.
///
/// Everything that is not a timeout or an out-of-range request is the device
/// refusing to answer correctly, which no caller above can distinguish or
/// retry differently.
impl From<VirtioError> for BlockError {
    fn from(error: VirtioError) -> Self {
        match error {
            VirtioError::Timeout => Self::Timeout,
            VirtioError::Dma(DmaError::Range) => Self::Range,
            _ => Self::Device,
        }
    }
}
