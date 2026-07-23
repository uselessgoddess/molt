//! Typed physical memory: what a span holds, who owns it, and what a mapping
//! of it may grant.
//!
//! [`Kind`] comes from firmware, [`Owner`] records the current holder, and
//! [`Rights`] plus [`Cache`] constrain mappings. A non-copy [`Frames`] token
//! makes each claim explicit and must be consumed to release it.

use core::ops::Range;

use crate::{FRAME_SIZE, MappingError, MemoryMap, MemoryRegionKind, PageProtection};

/// Failure while classifying, claiming, or releasing physical memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The bounds are not frame-aligned, or the span is empty or inverted.
    Misaligned,
    /// The span reaches outside the range the table or map covers.
    Range,
    /// The span covers more than one [`Kind`], so no single rule applies to it.
    Mixed,
    /// The span's kind does not allow what was asked of it.
    Kind,
    /// Some frame of the span already has an owner.
    Owned,
    /// The span is free, or held by a different owner than the one releasing it.
    NotOwner,
}

/// A frame-aligned, non-empty half-open range of physical memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    start: u64,
    end: u64,
}

impl Span {
    /// Rejects empty, inverted, or unaligned bounds.
    pub const fn new(start: u64, end: u64) -> Result<Self, Error> {
        if start >= end || start % FRAME_SIZE != 0 || end % FRAME_SIZE != 0 {
            return Err(Error::Misaligned);
        }
        Ok(Self { start, end })
    }

    /// The span of `count` frames starting at `start`.
    pub const fn frames(start: u64, count: u64) -> Result<Self, Error> {
        match start.checked_add(count * FRAME_SIZE) {
            Some(end) => Self::new(start, end),
            None => Err(Error::Range),
        }
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn end(self) -> u64 {
        self.end
    }

    pub const fn bytes(self) -> u64 {
        self.end - self.start
    }

    pub const fn count(self) -> u64 {
        self.bytes() / FRAME_SIZE
    }

    pub const fn contains(self, address: u64) -> bool {
        self.start <= address && address < self.end
    }

    pub const fn covers(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// What physical memory is, as opposed to what someone wants to do with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// RAM the firmware reported as usable.
    Ram,
    /// RAM the loader placed the kernel image in.
    Image,
    /// Claimed by firmware or the loader: neither allocatable nor mappable.
    Reserved,
    /// No firmware region covers it. Devices live in the holes of a memory
    /// map — RAM never does — so this is the only kind an MMIO window can have.
    Device,
}

impl Kind {
    /// Checks mapping rights and cache policy against this memory kind.
    pub const fn allows(self, rights: Rights, cache: Cache) -> Result<(), MappingError> {
        match self {
            Self::Ram | Self::Image => match cache {
                Cache::WriteBack => Ok(()),
                // Device ordering on RAM is valid but always unintended here.
                Cache::Device => Err(MappingError::Cacheability),
            },
            Self::Device => {
                if rights.is_execute() {
                    return Err(MappingError::Permissions);
                }
                match cache {
                    // Write-back caching does not preserve MMIO register semantics.
                    Cache::WriteBack => Err(MappingError::Cacheability),
                    Cache::Device => Ok(()),
                }
            }
            Self::Reserved => Err(MappingError::Permissions),
        }
    }
}

/// Rights a mapping grants, with W^X enforced at construction.
///
/// Unlike [`MapPermissions`](crate::MapPermissions), includes an explicit read bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rights {
    read: bool,
    write: bool,
    execute: bool,
}

impl Rights {
    pub const READ: Self = Self { read: true, write: false, execute: false };
    pub const READ_WRITE: Self = Self { read: true, write: true, execute: false };
    pub const READ_EXECUTE: Self = Self { read: true, write: false, execute: true };

    pub const fn new(read: bool, write: bool, execute: bool) -> Result<Self, MappingError> {
        if write && execute {
            return Err(MappingError::WritableExecutable);
        }
        // Neither target supports write-only or execute-only mappings.
        if !read {
            return Err(MappingError::Permissions);
        }
        Ok(Self { read, write, execute })
    }

    pub const fn page_protected(protect: PageProtection) -> Result<Self, MappingError> {
        Self::new(protect.is_read(), protect.is_write(), protect.is_execute())
    }

    pub const fn is_read(self) -> bool {
        self.read
    }

    pub const fn is_write(self) -> bool {
        self.write
    }

    pub const fn is_execute(self) -> bool {
        self.execute
    }
}

impl TryFrom<PageProtection> for Rights {
    type Error = MappingError;

    fn try_from(protect: PageProtection) -> Result<Self, Self::Error> {
        Self::page_protected(protect)
    }
}

/// Cache policies supported by current mapping consumers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Cache {
    /// Ordinary cacheable memory.
    #[default]
    WriteBack,
    /// Uncached and unspeculated: what an MMIO window must be mapped as.
    Device,
}

/// Who holds a span, using architecture-layer opaque identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Owner {
    /// General kernel data structures.
    Kernel,
    /// Live translation tables.
    Tables,
    /// A DMA buffer that may remain device-accessible without a CPU mapping.
    Device(u32),
    /// Memory a cell owns and loses on restart.
    Cell(u32),
}

/// A non-copy proof that a [`FrameTable`] assigned a span to one owner.
///
/// Release consumes the token; explicit release avoids requiring a global table
/// from `Drop`.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "claimed frames are leaked unless they are released or stored"]
pub struct Frames {
    span: Span,
    owner: Owner,
}

impl Frames {
    pub const fn span(&self) -> Span {
        self.span
    }

    pub const fn owner(&self) -> Owner {
        self.owner
    }
}

/// Per-frame ownership backed by caller-supplied storage.
pub struct FrameTable<'s> {
    base: Span,
    slots: &'s mut [Option<Owner>],
}

impl<'s> FrameTable<'s> {
    /// Tracks `base` with one slot per frame, rejecting undersized storage.
    pub fn over(base: Span, slots: &'s mut [Option<Owner>]) -> Result<Self, Error> {
        if (slots.len() as u64) < base.count() {
            return Err(Error::Range);
        }
        slots.fill(None);
        Ok(Self { base, slots })
    }

    pub const fn base(&self) -> Span {
        self.base
    }

    /// Claims every frame of `span` for `owner`, or nothing at all.
    pub fn claim(&mut self, span: Span, owner: Owner) -> Result<Frames, Error> {
        let range = self.range(span)?;
        if self.slots[range.clone()].iter().any(Option::is_some) {
            return Err(Error::Owned);
        }
        self.slots[range].fill(Some(owner));
        Ok(Frames { span, owner })
    }

    /// Releases frames issued by this table and rejects foreign tokens.
    pub fn release(&mut self, frames: Frames) -> Result<(), Error> {
        let range = self.range(frames.span)?;
        if self.slots[range.clone()].iter().any(|slot| *slot != Some(frames.owner)) {
            return Err(Error::NotOwner);
        }
        self.slots[range].fill(None);
        Ok(())
    }

    /// The owner of the frame containing `address`, if it has one.
    pub fn owner(&self, address: u64) -> Result<Option<Owner>, Error> {
        if !self.base.contains(address) {
            return Err(Error::Range);
        }
        Ok(self.slots[((address - self.base.start()) / FRAME_SIZE) as usize])
    }

    /// Returns the number of claimed frames.
    pub fn claimed(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    fn range(&self, span: Span) -> Result<Range<usize>, Error> {
        if !self.base.covers(span) {
            return Err(Error::Range);
        }
        let first = ((span.start() - self.base.start()) / FRAME_SIZE) as usize;
        Ok(first..first + span.count() as usize)
    }
}

/// Typed classification derived from the boot memory map.
///
/// Device windows can only come from holes outside RAM and the kernel image.
#[derive(Clone, Copy)]
pub struct Inventory<'m> {
    map: &'m dyn MemoryMap,
    image: Option<Span>,
}

impl<'m> Inventory<'m> {
    pub const fn new(map: &'m dyn MemoryMap) -> Self {
        Self { map, image: None }
    }

    /// Records the kernel image's physical range when the platform provides it.
    pub const fn with_image(mut self, image: Span) -> Self {
        self.image = Some(image);
        self
    }

    pub fn kind(&self, address: u64) -> Kind {
        if let Some(image) = self.image
            && image.contains(address)
        {
            return Kind::Image;
        }
        let mut index = 0;
        while index < self.map.len() {
            if let Some(region) = self.map.region(index)
                && region.start() <= address
                && address < region.end()
            {
                return match region.kind() {
                    MemoryRegionKind::Usable => Kind::Ram,
                    _ => Kind::Reserved,
                };
            }
            index += 1;
        }
        Kind::Device
    }

    /// Returns the common kind of `span`, or [`Error::Mixed`] at a boundary.
    pub fn classify(&self, span: Span) -> Result<Kind, Error> {
        let kind = self.kind(span.start());
        let mut address = span.start() + FRAME_SIZE;
        while address < span.end() {
            if self.kind(address) != kind {
                return Err(Error::Mixed);
            }
            address += FRAME_SIZE;
        }
        Ok(kind)
    }

    /// A device window at `span`, if firmware did not claim it as memory.
    ///
    /// [`Kind::Reserved`] qualifies alongside [`Kind::Device`]: e820 reports
    /// ECAM as an explicit reservation, a device tree leaves it as a hole —
    /// same window, two formats. [`Kind::Ram`] and [`Kind::Image`] stay refused.
    pub fn device(&self, span: Span) -> Result<Device, Error> {
        match self.classify(span)? {
            Kind::Device | Kind::Reserved => Ok(Device { span }),
            Kind::Ram | Kind::Image => Err(Error::Kind),
        }
    }
}

/// An MMIO span proven to lie outside firmware-described memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct Device {
    span: Span,
}

impl Device {
    pub const fn span(self) -> Span {
        self.span
    }

    /// Validates `rights` and returns the mandatory device cache policy.
    pub const fn mapping(self, rights: Rights) -> Result<(Rights, Cache), MappingError> {
        match Kind::Device.allows(rights, Cache::Device) {
            Ok(()) => Ok((rights, Cache::Device)),
            Err(error) => Err(error),
        }
    }
}
