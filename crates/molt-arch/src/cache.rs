//! Files, cached where they are already addressable.
//!
//! Reading a file costs two copies on a system with more than one address
//! space: the bytes land in the kernel's page cache, and then they land again
//! wherever the caller asked for them, because the caller's address for those
//! bytes is not the kernel's address for them. The second copy pays for the
//! disagreement, not for the file.
//!
//! Molt has one address space, so there is no disagreement to pay for. A window
//! of a file is read into frames once and given *an* address, and that address
//! is what the window is called everywhere — handing it to a domain adds a leaf
//! to that domain's view and moves no bytes. `copy_from_user` has nothing to do,
//! because the buffer is already at the address both ends name it by.
//!
//! What that buys shows up at size. A gigabyte-class window is one page-table
//! entry per gigabyte in every view that holds it, so a domain mapping a hundred
//! gigabytes of logs costs a hundred entries and one flush, rather than 26
//! million page-cache lookups and a copy per page.
//!
//! # What this module is and is not
//!
//! It is the bookkeeping: which windows are cached, at which addresses, over
//! which frames, and how many views hold each. Reading the bytes in is the
//! filesystem's, mapping them is [`Platform::grant`], and counting the leaves a
//! grant shares is [`refcount`] — this says only where a window is, so that the
//! second domain to ask for it is told the same address as the first.
//!
//! Nothing here allocates: a caller supplies the slice the windows live in, the
//! way [`Space`](crate::va::Space) is handed its holes.
//!
//! # Order
//!
//! A window's addresses are the allocator's and go back the way any other
//! extent does. [`evict`](Windows::evict) hands the [`Extent`] out rather than
//! dropping it, because the caller still owes the unmap, the shootdown, and the
//! retire, in that order — see [`view`](crate::view) for why the order is not
//! negotiable. It refuses while anybody still has the window mapped, which is
//! the one thing this module can enforce on its own.
//!
//! [`Platform::grant`]: crate::Platform::grant
//! [`refcount`]: crate::refcount

use crate::memory::Span;
use crate::va::{Class, Extent, Region};

/// Why a cache request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// No slot left to describe another window.
    Storage,
    /// The file offset is not on a boundary of the window's own leaf size.
    Misaligned,
    /// The frames do not cover the extent, or do not start on the boundary its
    /// class asks for, so no leaf could name them.
    Backing,
    /// That window of that file is cached already, at an address of its own.
    Present,
    /// No window of that file starts there.
    Unknown,
    /// Somebody still has it mapped.
    Held,
    /// Nobody holds it, so there is no reference to give back.
    Unreferenced,
    /// One more holder than a count can hold.
    Saturated,
}

/// Whose bytes a window holds.
///
/// A number and nothing else: which files exist is the filesystem's business,
/// and a page cache that knew would be a page cache with a filesystem inside
/// it. What this module needs is only that two requests for the same file agree
/// on the number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct File(u64);

impl File {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One window of one file: the addresses it is cached at, and the frames under
/// them.
///
/// The extent lives here rather than with whoever mapped it, because the whole
/// point is that the window outlives any one view's interest in it. It comes
/// back out through [`Windows::evict`].
#[derive(Debug, Eq, PartialEq)]
pub struct Window {
    file: File,
    offset: u64,
    extent: Option<Extent>,
    frames: Option<Span>,
    holders: u32,
}

impl Window {
    /// An unused slot, so a caller can write `[const { Window::EMPTY }; 16]`.
    pub const EMPTY: Self =
        Self { file: File::new(0), offset: 0, extent: None, frames: None, holders: 0 };

    /// Which file the bytes came from.
    pub const fn file(&self) -> File {
        self.file
    }

    /// Where in that file the window starts.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The addresses the window is cached at, or `None` for an unused slot.
    ///
    /// This is the answer the cache exists to give, and it is the same answer
    /// every time: a second domain asking for this window of this file is told
    /// this range, which is why the grant it gets copies nothing.
    pub const fn region(&self) -> Option<Region> {
        match self.extent {
            Some(ref extent) => Some(extent.region()),
            None => None,
        }
    }

    /// The extent itself, for the [`grant`](crate::Platform::grant) that maps it.
    pub const fn extent(&self) -> Option<&Extent> {
        self.extent.as_ref()
    }

    /// The frames the bytes were read into.
    pub const fn frames(&self) -> Option<Span> {
        self.frames
    }

    /// The leaf size the window is mapped with, which is also what its offset
    /// has to be a multiple of.
    pub const fn class(&self) -> Option<Class> {
        match self.extent {
            Some(ref extent) => Some(extent.class()),
            None => None,
        }
    }

    /// How many views have it mapped right now.
    pub const fn holders(&self) -> u32 {
        self.holders
    }

    /// How much of the address space the window covers.
    pub const fn bytes(&self) -> u64 {
        match self.extent {
            Some(ref extent) => extent.bytes(),
            None => 0,
        }
    }

    const fn is(&self, file: File, offset: u64) -> bool {
        self.extent.is_some() && self.file.0 == file.0 && self.offset == offset
    }
}

/// The windows this kernel has cached, and who holds them.
#[derive(Debug)]
pub struct Windows<'windows> {
    windows: &'windows mut [Window],
    len: usize,
    hits: u64,
    misses: u64,
}

impl<'windows> Windows<'windows> {
    /// Takes the slice the windows live in. Nothing is cached yet.
    ///
    /// Slots past what is cached are never read, so a caller reusing a slice
    /// hands over whatever the last cache left in it and loses nothing but the
    /// addresses it should have evicted first.
    pub fn over(windows: &'windows mut [Window]) -> Self {
        Self { windows, len: 0, hits: 0, misses: 0 }
    }

    /// Caches `extent` as the window of `file` at `offset`, backed by `frames`,
    /// and counts the caller as its first holder.
    ///
    /// Held from the start for the same reason [`Leaves::map`] counts one: a
    /// window is filled because somebody is mapping it, and a window nobody has
    /// ever held is a read the kernel did for no one.
    ///
    /// [`Leaves::map`]: crate::refcount::Leaves::map
    pub fn insert(
        &mut self,
        file: File,
        offset: u64,
        extent: Extent,
        frames: Span,
    ) -> Result<&Window, Error> {
        let granule = extent.class().granule();
        if offset % granule != 0 {
            return Err(Error::Misaligned);
        }
        if frames.bytes() < extent.bytes() || frames.start() % granule != 0 {
            return Err(Error::Backing);
        }
        if self.find(file, offset).is_some() {
            return Err(Error::Present);
        }
        if self.len == self.windows.len() {
            return Err(Error::Storage);
        }

        let at = self.len;
        self.windows[at] =
            Window { file, offset, extent: Some(extent), frames: Some(frames), holders: 1 };
        self.len += 1;
        Ok(&self.windows[at])
    }

    /// Takes a reference to a cached window, or says there is none.
    ///
    /// [`Error::Unknown`] is a miss and the caller's cue to find frames, read
    /// the bytes into them, and [`insert`](Self::insert) what it built. A hit is
    /// the whole saving: the bytes are already in RAM at an address that means
    /// the same thing in every view, so what is left to do is one leaf.
    pub fn hold(&mut self, file: File, offset: u64) -> Result<&Window, Error> {
        let Some(at) = self.find(file, offset) else {
            self.misses += 1;
            return Err(Error::Unknown);
        };

        // A saturated count is not a hit: handing the window out with the count
        // stuck would lose a reference, and a lost reference is what frees
        // memory somebody is still reading.
        let window = &mut self.windows[at];
        window.holders = window.holders.checked_add(1).ok_or(Error::Saturated)?;
        self.hits += 1;
        Ok(&self.windows[at])
    }

    /// The window without holding it, and without counting the look.
    pub fn lookup(&self, file: File, offset: u64) -> Option<&Window> {
        self.find(file, offset).map(|at| &self.windows[at])
    }

    /// Gives one holder's reference back, and says how many are left.
    ///
    /// Zero does not evict: the bytes stay cached at the same address, which is
    /// what makes the next domain to want them cost one leaf.
    pub fn release(&mut self, file: File, offset: u64) -> Result<u32, Error> {
        let at = self.find(file, offset).ok_or(Error::Unknown)?;
        let window = &mut self.windows[at];
        window.holders = window.holders.checked_sub(1).ok_or(Error::Unreferenced)?;
        Ok(window.holders)
    }

    /// Drops the window, handing back what the caller still owes work on.
    ///
    /// The addresses are not free when this returns — the leaves are gone from
    /// the tables only once the caller unmaps them, and the range is nobody's to
    /// hand out until every core has flushed. That is why the [`Extent`] comes
    /// out rather than going away: it cannot be released without a
    /// [`Space`](crate::va::Space), and it is loud about being dropped.
    pub fn evict(&mut self, file: File, offset: u64) -> Result<(Extent, Span), Error> {
        let at = self.find(file, offset).ok_or(Error::Unknown)?;
        if self.windows[at].holders != 0 {
            return Err(Error::Held);
        }

        let evicted = core::mem::replace(&mut self.windows[at], Window::EMPTY);
        self.len -= 1;
        self.windows.swap(at, self.len);
        match (evicted.extent, evicted.frames) {
            (Some(extent), Some(frames)) => Ok((extent, frames)),
            // Unreachable while every cached slot is filled by `insert`, which
            // writes both or neither.
            _ => Err(Error::Unknown),
        }
    }

    /// How many holds found a window already cached.
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// How many did not, which is how many reads the device actually saw.
    pub const fn misses(&self) -> u64 {
        self.misses
    }

    /// How many windows are cached.
    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How much of the address space they cover between them.
    pub fn bytes(&self) -> u64 {
        self.windows[..self.len].iter().map(Window::bytes).sum()
    }

    /// Every cached window, in no order a caller should rely on.
    pub fn iter(&self) -> impl Iterator<Item = &Window> {
        self.windows[..self.len].iter()
    }

    fn find(&self, file: File, offset: u64) -> Option<usize> {
        self.windows[..self.len].iter().position(|window| window.is(file, offset))
    }
}
