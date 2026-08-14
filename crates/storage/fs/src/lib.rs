//! MoltFS, a checksummed writable filesystem over [`molt_block::Disk`].
//!
//! `xtask mkfs` creates generation one of the same format runtime mutations
//! use: objects, directory entries, and extents are typed keys in a checksummed
//! copy-on-write B+ tree, while file bytes live in one of three rotating payload
//! banks. A sync flushes both before publishing their root through the older of
//! two generation-stamped superblocks, then flushes the superblock. Power loss
//! therefore leaves either the previous generation or the complete new
//! generation mountable, without fsck. Records carry per-chunk checksums and
//! append in the active bank until it fills; then live extent slices stream
//! into the free bank and stale writes are reclaimed.
//!
//! [`Volume`] selects checkpoints and supplies block I/O. It never calls a
//! device: [`attach`] puts a block ring under it, so a read submits and awaits.
//! [`Journal`] validates the typed index, reads payloads with readahead, and
//! applies mutations; [`Fs`] wraps it in the ring
//! protocol every other cell talks: typed [`FsOp`] submissions in, [`FsDone`]
//! completions out, with directories and files named by capability rather than
//! by path. Metadata nodes and the arena bitmap live on the heap, so a mutation
//! costs its caller a path of block numbers rather than kilobytes of frame.
//!
//! See `docs/fs.md` for the format and the decisions behind it.

#![no_std]
#![feature(allocator_api)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::alloc::AllocError;
use alloc::collections::TryReserveError;

use molt_block::BlockError;
use molt_core::buffer::BufferError;
use molt_core::capability::CapabilityError;
use molt_core::registry::RegistryError;

mod bitmap;
mod btree;
mod cell;
mod crc;
mod journal;
mod layout;
mod log;
mod mem;
mod name;
mod op;
mod restart;
mod service;
mod storage;
mod volume;

#[cfg(feature = "format")]
pub mod format;

pub use crate::btree::{CacheStats, TreeStats};
pub use crate::cell::FsCell;
pub use crate::journal::Journal;
pub use crate::layout::{BLOCK, Kind, MAGIC, MAX_NAME, Object, SUPERS, VERSION};
pub use crate::name::Name;
pub use crate::op::{Dir, File, FsDone, FsOp, Handle, Stat};
pub use crate::restart::{Disconnect, Teardown};
pub use crate::service::Fs;
pub use crate::storage::{Mount, Storage};
pub use crate::volume::{Blocks, DEPTH, Volume, attach};

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
    /// The service restarted while the request was on the ring, so nothing ran
    /// it. Nothing it would have changed happened.
    Cancelled,
    /// The namespace could not publish the mount, so nobody can acquire it.
    Namespace(RegistryError),
    /// A handle that is unknown, stale, or short of rights.
    Handle(CapabilityError),
    /// A buffer that is unknown or does not hold the range claimed for it.
    Buffer(BufferError),
    /// No free handle left in the table.
    Handles,
    /// The tree arena, mutation log, or object-id space is full.
    Full,
    /// The service ran its restart hooks and could not remount, so there is no
    /// filesystem behind it any more.
    Failed,
    /// The heap refused memory the operation needed.
    Memory,
}

impl From<AllocError> for FsError {
    fn from(_: AllocError) -> Self {
        Self::Memory
    }
}

impl From<TryReserveError> for FsError {
    fn from(_: TryReserveError) -> Self {
        Self::Memory
    }
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

impl From<RegistryError> for FsError {
    fn from(error: RegistryError) -> Self {
        Self::Namespace(error)
    }
}
