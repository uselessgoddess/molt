//! Files, cached where they are already addressable.
//!
//! With more than one address space, reading a file costs two copies: into the
//! page cache, then into the caller, because the caller's address for those
//! bytes is not the kernel's. Molt has one address space, so a window is read
//! into frames once and handing its address to a domain adds a leaf and moves
//! nothing. A hundred gigabytes of logs is a hundred gigabyte-class entries and
//! one flush, not 26 million page-cache lookups and a copy per page.
//!
//! Bookkeeping only — which windows are cached, at which addresses, over which
//! frames, and how many views hold each. Reading the bytes is the filesystem's
//! job, mapping them is [`Platform::grant`], counting the leaves is
//! [`refcount`], and nothing here allocates.
//!
//! [`evict`](Windows::evict) refuses while anybody still has the window mapped,
//! and hands the [`Extent`] out rather than dropping it: the caller still owes
//! the unmap, the shootdown and the retire, in that order ([`view`]).
//!
//! [`Platform::grant`]: crate::Platform::grant
//! [`refcount`]: crate::refcount
//! [`view`]: crate::view

use crate::memory::Span;
use crate::va::{Extent, Region};

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
/// A number and nothing else: all this module needs is that two requests for
/// one file agree on it. Which files exist is the filesystem's business.
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
/// The extent lives here rather than with whoever mapped it, because a window
/// outlives any one view's interest in it. It leaves through
/// [`Windows::evict`].
#[derive(Debug, Eq, PartialEq)]
pub struct Window {
    file: File,
    offset: u64,
    /// One field, because a window has both or is an unused slot; apart, a slot
    /// could hold one without the other and every reader would owe that an
    /// answer.
    backing: Option<(Extent, Span)>,
    holders: u32,
}

impl Window {
    /// An unused slot, so a caller can write `[const { Window::EMPTY }; 16]`.
    pub const EMPTY: Self = Self { file: File::new(0), offset: 0, backing: None, holders: 0 };

    /// The addresses the window is cached at, or `None` for an unused slot.
    ///
    /// The same answer every time, which is why a second domain asking for this
    /// window gets a grant that copies nothing.
    pub fn region(&self) -> Option<Region> {
        self.extent().map(Extent::region)
    }

    /// The extent itself, for the [`grant`](crate::Platform::grant) that maps it.
    pub fn extent(&self) -> Option<&Extent> {
        self.backing.as_ref().map(|(extent, _)| extent)
    }

    /// How many views have it mapped right now.
    pub const fn holders(&self) -> u32 {
        self.holders
    }

    /// How much of the address space the window covers.
    pub fn bytes(&self) -> u64 {
        self.extent().map_or(0, Extent::bytes)
    }

    const fn is(&self, file: File, offset: u64) -> bool {
        self.backing.is_some() && self.file.0 == file.0 && self.offset == offset
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
    pub fn over(windows: &'windows mut [Window]) -> Self {
        Self { windows, len: 0, hits: 0, misses: 0 }
    }

    /// Caches `extent` as the window of `file` at `offset`, backed by `frames`,
    /// and counts the caller as its first holder — for the same reason
    /// [`Leaves::map`] counts one: a window is filled because somebody is
    /// mapping it.
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
        self.windows[at] = Window { file, offset, backing: Some((extent, frames)), holders: 1 };
        self.len += 1;
        Ok(&self.windows[at])
    }

    /// Takes a reference to a cached window, or says there is none.
    ///
    /// [`Error::Unknown`] is a miss and the caller's cue to find frames, read
    /// the bytes into them, and [`insert`](Self::insert) what it built. A hit
    /// leaves one leaf to map and nothing to copy.
    pub fn hold(&mut self, file: File, offset: u64) -> Result<&Window, Error> {
        let Some(at) = self.find(file, offset) else {
            self.misses += 1;
            return Err(Error::Unknown);
        };

        // A saturated count is not a hit: handing the window out with the count
        // stuck would lose a reference, and lose the memory somebody is reading.
        let window = &mut self.windows[at];
        window.holders = window.holders.checked_add(1).ok_or(Error::Saturated)?;
        self.hits += 1;
        Ok(&self.windows[at])
    }

    /// The window without holding it, and without counting the look.
    pub fn lookup(&self, file: File, offset: u64) -> Option<&Window> {
        self.find(file, offset).map(|at| &self.windows[at])
    }

    /// Gives one holder's reference back, and says how many are left. Zero does
    /// not evict: the bytes stay cached, so the next domain to want them costs
    /// one leaf.
    pub fn release(&mut self, file: File, offset: u64) -> Result<u32, Error> {
        let at = self.find(file, offset).ok_or(Error::Unknown)?;
        let window = &mut self.windows[at];
        window.holders = window.holders.checked_sub(1).ok_or(Error::Unreferenced)?;
        Ok(window.holders)
    }

    /// Drops the window, handing back what the caller still owes work on.
    ///
    /// The addresses are not free when this returns: the leaves go when the
    /// caller unmaps them, and the range is nobody's until every core has
    /// flushed. Hence the [`Extent`] coming out rather than going away — it
    /// cannot be released without a [`Space`](crate::va::Space), and it is loud
    /// about being dropped.
    pub fn evict(&mut self, file: File, offset: u64) -> Result<(Extent, Span), Error> {
        let at = self.find(file, offset).ok_or(Error::Unknown)?;
        if self.windows[at].holders != 0 {
            return Err(Error::Held);
        }

        let evicted = core::mem::replace(&mut self.windows[at], Window::EMPTY);
        self.len -= 1;
        self.windows.swap(at, self.len);
        // `find` matches filled slots only, so this is the window it named.
        evicted.backing.ok_or(Error::Unknown)
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

    fn find(&self, file: File, offset: u64) -> Option<usize> {
        self.windows[..self.len].iter().position(|window| window.is(file, offset))
    }
}
