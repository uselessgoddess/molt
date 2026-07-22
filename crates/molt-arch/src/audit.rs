//! Exhaustive audit of a live address space against what the kernel declared.
//!
//! [`Audit::cover`] checks every declared page; [`Audit::accepts`] rejects
//! undeclared or over-wide leaves when the kernel can walk the complete table.

use crate::memory::{Kind, Rights};
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
    /// A named image section with exact rights and 4 KiB leaves.
    Section(ImageSection),
    /// An image span with W^X and 4 KiB leaves but unknown section bounds.
    Image,
    /// Writable, non-executable RAM at any leaf size.
    Ram,
    /// Uncacheable, non-executable MMIO.
    Device,
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
            Self::Device => {
                let rights = match Rights::page_protected(granted) {
                    Ok(rights) => rights,
                    Err(error) => return Err(error),
                };
                Kind::Device.allows(rights, granted.cache())
            }
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

    pub const fn device(start: u64, end: u64) -> Self {
        Self::new(start, end, Contents::Device)
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

/// Fixed-capacity declared ranges, coalesced by [`push`](Self::push).
#[derive(Clone, Copy, Debug)]
pub struct Declared<const N: usize> {
    ranges: [MappedRange; N],
    len: usize,
}

impl<const N: usize> Declared<N> {
    pub const fn new() -> Self {
        const EMPTY: MappedRange = MappedRange::new(0, 0, Contents::Ram);
        Self { ranges: [EMPTY; N], len: 0 }
    }

    /// Records `range`, extending the previous entry when the two are adjacent.
    pub fn push(&mut self, range: MappedRange) -> Result<(), MappingError> {
        if range.start >= range.end {
            return Ok(());
        }
        if let Some(last) = self.ranges[..self.len].last_mut()
            && last.contents == range.contents
            && last.end == range.start
        {
            last.end = range.end;
            return Ok(());
        }
        if self.len == N {
            return Err(MappingError::Backend);
        }
        self.ranges[self.len] = range;
        self.len += 1;
        Ok(())
    }

    pub fn as_slice(&self) -> &[MappedRange] {
        &self.ranges[..self.len]
    }

    pub fn audit(&self) -> Audit<'_> {
        Audit::new(self.as_slice())
    }
}

impl<const N: usize> Default for Declared<N> {
    fn default() -> Self {
        Self::new()
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
                // Reject a malformed walk before it can stall the audit.
                if leaf.end() <= address {
                    return Err(MappingError::Backend);
                }
                range.contents.verify(leaf)?;
                address = leaf.end();
            }
        }
        Ok(())
    }

    /// Checks that a live leaf lies within and obeys one declared range.
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
