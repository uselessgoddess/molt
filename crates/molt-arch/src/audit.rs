//! Exhaustive audit of a live address space against what the kernel declared.
//!
//! Probing a handful of addresses proves those addresses are right and says
//! nothing about the pages between them: a `.text` page mapped writable by a
//! stray megapage sits happily between two correct probes. An audit instead
//! walks every page of every declared range, and — where the kernel owns the
//! tables — every leaf the tables hold, so the mapped set and the declared set
//! have to be the same set.
//!
//! [`Audit::cover`] is the outward direction: everything declared is mapped
//! with exactly the rights its contents allow. [`Audit::accepts`] is the
//! inward one: nothing else is mapped at all, and no large leaf reaches past
//! the range it belongs to. Firmware-owned tables can only afford the first.

use crate::{FRAME_SIZE, ImageSection, MappingError, PageProtection};

/// One leaf entry of a live translation table: what it covers and what it grants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Leaf {
    start: u64,
    size: u64,
    protection: PageProtection,
}

impl Leaf {
    pub const fn new(start: u64, size: u64, protection: PageProtection) -> Self {
        Self { start, size, protection }
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn size(self) -> u64 {
        self.size
    }

    pub const fn protection(self) -> PageProtection {
        self.protection
    }

    /// One past the last address the leaf translates.
    pub const fn end(self) -> u64 {
        self.start.saturating_add(self.size)
    }
}

/// Reads a live translation table back, one leaf at a time.
pub trait PageWalk {
    /// The leaf translating `address`, or `None` when nothing does.
    fn leaf(&self, address: u64) -> Option<Leaf>;
}

/// What a declared range holds, and therefore what its pages may grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Contents {
    /// A named image section: exactly its rights, on 4 KiB leaves, because a
    /// larger leaf would have to share rights with the next section.
    Section(ImageSection),
    /// A loaded image whose section bounds the kernel cannot name, only the
    /// span: W^X and 4 KiB leaves, whichever section a page belongs to.
    Image,
    /// Free RAM: readable and writable, never executable. A megapage leaf is
    /// welcome here — the whole range carries one set of rights.
    Ram,
}

impl Contents {
    /// Checks one leaf's live rights and size against what these contents allow.
    pub const fn verify(self, leaf: Leaf) -> Result<(), MappingError> {
        let granted = leaf.protection;
        match self {
            Self::Section(section) => {
                if leaf.size != FRAME_SIZE {
                    return Err(MappingError::Granularity);
                }
                section.verify(granted)
            }
            Self::Image => {
                if leaf.size != FRAME_SIZE {
                    return Err(MappingError::Granularity);
                }
                if granted.is_write() && granted.is_execute() {
                    return Err(MappingError::WritableExecutable);
                }
                if granted.is_read() { Ok(()) } else { Err(MappingError::Permissions) }
            }
            Self::Ram => ImageSection::Data.verify(granted),
        }
    }
}

/// A virtual range the kernel declares it has mapped, and what it holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedRange {
    start: u64,
    end: u64,
    contents: Contents,
}

impl MappedRange {
    pub const fn new(start: u64, end: u64, contents: Contents) -> Self {
        Self { start, end, contents }
    }

    pub const fn section(section: ImageSection, start: u64, end: u64) -> Self {
        Self::new(start, end, Contents::Section(section))
    }

    pub const fn image(start: u64, end: u64) -> Self {
        Self::new(start, end, Contents::Image)
    }

    pub const fn ram(start: u64, end: u64) -> Self {
        Self::new(start, end, Contents::Ram)
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn end(self) -> u64 {
        self.end
    }

    pub const fn contents(self) -> Contents {
        self.contents
    }
}

/// The ranges a kernel claims to have mapped, checked against the live tables.
#[derive(Clone, Copy, Debug)]
pub struct Audit<'ranges> {
    ranges: &'ranges [MappedRange],
}

impl<'ranges> Audit<'ranges> {
    pub const fn new(ranges: &'ranges [MappedRange]) -> Self {
        Self { ranges }
    }

    /// Walks every page of every declared range and checks its live rights.
    pub fn cover<W: PageWalk + ?Sized>(&self, walk: &W) -> Result<(), MappingError> {
        for range in self.ranges {
            let mut address = range.start;
            while address < range.end {
                let leaf = walk.leaf(address).ok_or(MappingError::Unmapped)?;
                // A walk that reports a leaf ending at or before the address it
                // was asked about would spin here forever.
                if leaf.end() <= address {
                    return Err(MappingError::Backend);
                }
                range.contents.verify(leaf)?;
                address = leaf.end();
            }
        }
        Ok(())
    }

    /// Checks a leaf found in the live tables against the declared ranges: it
    /// must lie wholly inside one of them and grant what that one allows.
    pub fn accepts(&self, leaf: Leaf) -> Result<(), MappingError> {
        for range in self.ranges {
            if leaf.start < range.start || leaf.start >= range.end {
                continue;
            }
            if leaf.end() > range.end {
                return Err(MappingError::Straddling);
            }
            return range.contents.verify(leaf);
        }
        Err(MappingError::Unexpected)
    }
}
