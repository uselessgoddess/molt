//! MoltROFS, a read-only filesystem sitting on any [`molt_block::Device`].
//!
//! An image is written once by `xtask mkfs` and never modified, so the crate
//! carries no allocator, no journal, and no write path. What it does carry is
//! the parts a writable successor needs: a generation-stamped superblock kept
//! in two copies, a checksum over every metadata region, and a crc32c over
//! every data block. A checkpoint that overwrites the older copy and only then
//! becomes the newer one is the whole of crash consistency here — a torn write
//! leaves the previous checkpoint intact, and [`Volume::mount`] takes the
//! newest copy that verifies.
//!
//! [`Volume`] is the reader, needing one block of buffer and nothing else.
//! [`Fs`] wraps it in the ring protocol every other cell talks: typed [`FsOp`]
//! submissions in, [`FsDone`] completions out, with directories and files named
//! by capability rather than by path.
//!
//! See `docs/fs.md` for the format and the decisions behind it.

#![no_std]

#[cfg(feature = "format")]
extern crate alloc;
#[cfg(test)]
extern crate std;

use molt_block::BlockError;
use molt_core::buffer::BufferError;
use molt_core::capability::CapabilityError;

mod crc;
mod layout;
mod name;
mod op;
mod service;
mod volume;

#[cfg(feature = "format")]
pub mod format;
#[cfg(feature = "format")]
mod store;

pub use crate::layout::{BLOCK, Kind, MAGIC, MAX_NAME, Object, SUPERS, VERSION};
pub use crate::name::Name;
pub use crate::op::{Dir, File, FsDone, FsOp, Handle, Stat};
pub use crate::service::{Backend, Fs};
#[cfg(feature = "format")]
pub use crate::store::Store;
pub use crate::volume::Volume;

/// Why a filesystem operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    /// No volume signature where one was expected.
    Magic,
    /// A volume written in a format this build does not read.
    Version(u32),
    /// A checksum did not match the bytes it covers.
    Checksum,
    /// A structurally impossible volume: overlapping, truncated, or absurd.
    Corrupt,
    /// No such object, entry, or name.
    Missing,
    /// A name that is empty, overlong, or holds a separator.
    Name,
    /// A directory operation on a file, or the reverse.
    Kind,
    /// A name a directory already holds.
    Exists,
    /// A checkpoint larger than the arena that has to hold it.
    Full,
    /// A write aimed at a volume that only reads.
    ReadOnly,
    /// An offset past the end of what it addresses.
    Range,
    /// The device below refused the read.
    Device(BlockError),
    /// A root grant asked for after the bootstrap was sealed.
    Sealed,
    /// A handle that is unknown, stale, or short of rights.
    Handle(CapabilityError),
    /// A buffer that is unknown or does not hold the range claimed for it.
    Buffer(BufferError),
    /// No free handle left in the table.
    Handles,
}

impl From<BlockError> for FsError {
    fn from(error: BlockError) -> Self {
        Self::Device(error)
    }
}

impl From<CapabilityError> for FsError {
    fn from(error: CapabilityError) -> Self {
        Self::Handle(error)
    }
}

impl From<BufferError> for FsError {
    fn from(error: BufferError) -> Self {
        Self::Buffer(error)
    }
}
