//! MoltFS, a checksummed writable filesystem over [`molt_block::Writable`].
//!
//! `xtask mkfs` lays out immutable objects, extents, entries, names, sums, and
//! data. Runtime creates and writes are typed records in one of three rotating
//! log banks. A sync flushes a complete bank before publishing it through the
//! older of two generation-stamped superblocks, then flushes the superblock.
//! Power loss therefore leaves either the previous generation or the complete
//! new generation mountable, without fsck.
//!
//! [`Volume`] is the reader, needing one block of buffer and nothing else.
//! [`Journal`] adds allocation-free replay and mutation, and [`Fs`] wraps it in
//! the ring protocol every other cell talks: typed [`FsOp`] submissions in,
//! [`FsDone`] completions out, with directories and files named by capability
//! rather than by path.
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
mod journal;
mod layout;
mod log;
mod name;
mod op;
mod service;
mod volume;

#[cfg(feature = "format")]
pub mod format;

pub use crate::journal::Journal;
pub use crate::layout::{BLOCK, Kind, MAGIC, MAX_NAME, Object, SUPERS, VERSION};
pub use crate::name::Name;
pub use crate::op::{Dir, File, FsDone, FsOp, Handle, Stat};
pub use crate::service::Fs;
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
    /// The name already exists in that directory.
    Exists,
    /// A name that is empty, overlong, or holds a separator.
    Name,
    /// A directory operation on a file, or the reverse.
    Kind,
    /// An offset past the end of what it addresses.
    Range,
    /// The device below refused an operation.
    Device(BlockError),
    /// A root grant asked for after the bootstrap was sealed.
    Sealed,
    /// A handle that is unknown, stale, or short of rights.
    Handle(CapabilityError),
    /// A buffer that is unknown or does not hold the range claimed for it.
    Buffer(BufferError),
    /// No free handle left in the table.
    Handles,
    /// The mutation log or object-id space is full.
    Full,
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
