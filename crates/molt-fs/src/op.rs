//! The operations a filesystem ring carries.
//!
//! There are no paths and no current directory: an operation names what it
//! acts on by capability, and the only way to obtain one is to have opened it
//! from a directory somebody handed you. A cell holding a capability to one
//! subtree cannot address anything outside it, which is what a chroot is for
//! elsewhere and what the type is here.
//!
//! Data never travels in an operation. A read names a registered buffer, which
//! only the supervisor-owned registry can turn into memory, so the driver
//! writes into the client's buffer without either side handing out a pointer.

use molt_core::buffer::BufferOperation;
use molt_core::capability::{Capability, CapabilityRights, Rights, Write};

use crate::layout::Kind;
use crate::name::Name;

/// The rights an open directory carries.
///
/// A directory is a distinct type from a file so an operation that only makes
/// sense on one cannot be written for the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dir {}

impl CapabilityRights for Dir {
    const MASK: Rights = Rights::READ;
}

/// The rights an open file carries. A volume is read-only, so reading is all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum File {}

impl CapabilityRights for File {
    const MASK: Rights = Rights::READ;
}

/// An open handle of either kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Handle {
    Dir(Capability<Dir>),
    File(Capability<File>),
}

impl Handle {
    /// The directory this handle names, if it names one.
    pub const fn dir(self) -> Option<Capability<Dir>> {
        match self {
            Self::Dir(dir) => Some(dir),
            Self::File(_) => None,
        }
    }

    /// The file this handle names, if it names one.
    pub const fn file(self) -> Option<Capability<File>> {
        match self {
            Self::File(file) => Some(file),
            Self::Dir(_) => None,
        }
    }
}

/// One filesystem operation.
///
/// There is no operation for the root directory: a capability comes only from
/// opening it through one already held, and the first is handed off the ring at
/// bootstrap (see [`Fs::root`](crate::Fs::root)). Nothing a client can submit
/// mints authority the client did not already have.
// Only `Open` carries a `Name`, so the variants differ in size — but a boxed
// name would mean an allocator this layer refuses and a pointer crossing the
// ring, which is exactly what the inline name exists to avoid. The `const _`
// below pins the size the imbalance is allowed to reach.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsOp {
    /// Opens `name` inside `dir`, whichever kind it turns out to be.
    Open { dir: Capability<Dir>, name: Name },
    /// Reads `dir`'s entry at `index`, in name order.
    Entry { dir: Capability<Dir>, index: u32 },
    /// Reads `file` at `offset` into a registered buffer.
    Read { file: Capability<File>, buffer: BufferOperation<Write>, offset: u64 },
    /// Asks what a handle refers to.
    Stat(Handle),
    /// Drops a handle, freeing its slot.
    Close(Handle),
}

/// What an object is: a file's length, or a directory's entry count.
///
/// A listing carries this per entry because the volume has already read the
/// object record to answer at all — asking again through [`FsOp::Stat`] would
/// cost a round trip per name for something the first answer knew.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub entries: u32,
}

/// What an operation produced.
// `Entry` carries a `Name` the other answers do not; the imbalance is the
// inline name again, and boxing it would trade the ring's freedom from
// pointers for an allocator this layer does not have.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsDone {
    /// A handle to what was opened.
    Opened(Handle),
    /// One directory entry: its name and what it names.
    Entry { name: Name, stat: Stat },
    /// How many bytes landed in the buffer; short only at the end of a file.
    Read(usize),
    /// What a handle refers to.
    Stat(Stat),
    /// The handle is gone.
    Closed,
}

impl FsDone {
    /// The handle an open produced, if this is what an open produced.
    pub const fn handle(self) -> Option<Handle> {
        match self {
            Self::Opened(handle) => Some(handle),
            _ => None,
        }
    }
}

// A ring slot is copied by value on submission and again on completion, so its
// size is a per-operation cost. `Name`, at [`MAX_NAME`](crate::MAX_NAME) + 1
// bytes, dominates both messages; the bound leaves room for a header without
// letting either grow to something a stack-built ring would feel.
const _: () = assert!(core::mem::size_of::<FsOp>() <= 512);
const _: () = assert!(core::mem::size_of::<FsDone>() <= 512);
