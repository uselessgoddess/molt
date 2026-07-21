//! Typed physical memory: what a span holds, who owns it, and what a mapping
//! of it may grant.
//!
//! Stage 1 treated physical memory as addresses. [`FrameAllocator`] handed out
//! a `u64` and nothing recorded what happened to it afterwards, which is
//! survivable while the only consumer is the boot page table and fatal once a
//! driver, a DMA engine, and a ring all want frames from the same pool.
//!
//! Three things are separated here, because collapsing them is what makes a
//! memory model hard to reason about later:
//!
//! - [`Kind`] is what physical memory *is* — RAM the firmware handed out, the
//!   loaded image, firmware's own reservations, or a hole where devices live.
//!   It comes from the boot memory map and never from a caller's opinion.
//! - [`Owner`] is who holds a span *now*. It is metadata the kernel maintains
//!   in a [`FrameTable`], and it is what makes "this frame is already the page
//!   table's" a returned error rather than a silent double allocation.
//! - [`Rights`] and [`Cache`] are what a *mapping* of that memory may grant.
//!   They are checked against the kind, so `Kind::Device` cannot be mapped
//!   executable or write-back no matter which caller asks.
//!
//! [`Frames`] is the ownership token that ties the three together: it can only
//! come from [`FrameTable::claim`], it is not `Copy`, and returning the memory
//! consumes it. That is deliberately Theseus's discipline (a mapping is a
//! value, not an address) with seL4's rule at the edge (memory is typed before
//! it is handed to something the compiler cannot check), and no CSpace.

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
    /// Rejects anything the rest of this module would otherwise have to
    /// re-check: an empty span, an inverted one, or one whose bounds do not
    /// name whole frames.
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
    /// Checks a requested mapping of this kind of memory against its rules.
    ///
    /// The invariants are the reason this returns `Result` rather than the
    /// caller assembling flags: W^X is already enforced by [`Rights`], and
    /// what is left is the part a driver gets wrong — an executable or
    /// write-back MMIO window, and a mapping of memory that belongs to
    /// firmware.
    pub const fn allows(self, rights: Rights, cache: Cache) -> Result<(), MappingError> {
        match self {
            Self::Ram | Self::Image => match cache {
                Cache::WriteBack => Ok(()),
                // Device memory ordering for RAM is not a safety bug, but it
                // is never what the caller meant, and it costs a great deal.
                Cache::Device => Err(MappingError::Cacheability),
            },
            Self::Device => {
                if rights.is_execute() {
                    return Err(MappingError::Permissions);
                }
                match cache {
                    // A write-back MMIO window turns a register write into a
                    // store that reaches the device whenever a cache line
                    // happens to be evicted, if at all.
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
/// This is [`MapPermissions`](crate::MapPermissions) with the read bit made
/// explicit, because device and DMA mappings need to say "readable, not
/// writable" and Stage 1 only ever needed "writable, not executable".
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
        // A write-only or execute-only mapping is expressible on neither of
        // Molt's targets, so accepting one here would only move the surprise.
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

/// The cacheability a mapping asks the MMU for.
///
/// Two values, because the third useful one (write-combining framebuffers)
/// has no consumer yet and an unused enum variant is an untested one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Cache {
    /// Ordinary cacheable memory.
    #[default]
    WriteBack,
    /// Uncached and unspeculated: what an MMIO window must be mapped as.
    Device,
}

/// Who holds a span of physical memory.
///
/// The identifiers are opaque `u32`s rather than `molt-core` types because
/// `molt-arch` is the layer below it; the kernel supplies whatever a
/// `CellId` or device index means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Owner {
    /// General kernel data structures.
    Kernel,
    /// Translation tables. Named separately because releasing one while it is
    /// live unmaps memory that is being used to unmap memory.
    Tables,
    /// A DMA buffer a device can write to whether or not the CPU still maps it.
    Device(u32),
    /// Memory a cell owns and loses on restart.
    Cell(u32),
}

/// A claimed span of physical memory, and the proof that it was claimed.
///
/// There is no constructor: a `Frames` can only come from
/// [`FrameTable::claim`], it does not implement `Copy` or `Clone`, and
/// [`FrameTable::release`] consumes it. So the compiler, not a convention,
/// is what stops the same frames being handed to two owners.
///
/// It has no `Drop` on purpose. Releasing needs the table, a `Drop` cannot
/// reach one without a global, and a global frame table is the thing this
/// stage is trying not to need yet. `#[must_use]` catches the case that
/// matters — memory claimed and immediately dropped on the floor.
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

/// Per-frame ownership over one span of physical memory.
///
/// The storage is a caller-supplied slice, one entry per frame, so the table
/// costs exactly what the caller decided to track and `molt-arch` still
/// allocates nothing. Tracking all of RAM is one static array; tracking the
/// frames a driver may be handed is a much smaller one, and the choice
/// belongs to the kernel rather than to this type.
pub struct FrameTable<'s> {
    base: Span,
    slots: &'s mut [Option<Owner>],
}

impl<'s> FrameTable<'s> {
    /// Tracks `base`, using one slot per frame.
    ///
    /// Extra slots are accepted and ignored; too few are an error, because a
    /// table that silently covers less than it claims would report a frame
    /// outside its slice as free.
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

    /// Returns claimed frames to the free pool.
    ///
    /// The owner check cannot fail for frames this table issued; it exists so
    /// that frames issued by a *different* table are rejected instead of
    /// clearing slots that describe unrelated memory.
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

    /// How many frames are currently claimed, for a boot-time audit line.
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

/// The boot memory map read as typed physical memory rather than as regions.
///
/// This is what makes "map me this physical address" impossible to write: a
/// caller asks the inventory what a span is, and the inventory answers from
/// firmware's map. A driver that wants an MMIO window gets one only where
/// firmware left a hole, and never inside RAM or the kernel image.
#[derive(Clone, Copy)]
pub struct Inventory<'m> {
    map: &'m dyn MemoryMap,
    image: Option<Span>,
}

impl<'m> Inventory<'m> {
    pub const fn new(map: &'m dyn MemoryMap) -> Self {
        Self { map, image: None }
    }

    /// Records where the loader placed the kernel image, in physical addresses.
    ///
    /// A platform whose image range is only known virtually leaves this unset;
    /// the image then classifies as [`Kind::Ram`], which is weaker but not
    /// wrong, and the frame allocator's floor still keeps it out of the pool.
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

    /// The kind of every frame in `span`, when they agree.
    ///
    /// A span that straddles two kinds is [`Error::Mixed`] rather than the
    /// kind of its first frame: a request covering RAM *and* a device hole is
    /// a bug in the caller, and answering it would map one of the two wrong.
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
    /// Both [`Kind::Device`] and [`Kind::Reserved`] qualify, because the two
    /// are the same fact reported by two firmwares. A device tree describes
    /// ECAM by listing RAM and leaving the window out, so it arrives here as a
    /// hole — [`Kind::Device`]. An e820 map describes the identical window with
    /// an explicit non-usable entry, so it arrives as [`Kind::Reserved`].
    /// Accepting only the hole would make the x86_64 PCI path fail on every
    /// machine whose firmware is more, not less, informative.
    ///
    /// [`Kind::Ram`] and [`Kind::Image`] stay refused, which is the check that
    /// matters: those are the spans a driver must never reach through an
    /// uncached window.
    pub fn device(&self, span: Span) -> Result<Device, Error> {
        match self.classify(span)? {
            Kind::Device | Kind::Reserved => Ok(Device { span }),
            Kind::Ram | Kind::Image => Err(Error::Kind),
        }
    }
}

/// An MMIO window: a span the firmware map does not claim as RAM.
///
/// Carrying the proof in a type means a mapping routine takes a `Device` and
/// not a `u64`, so "the driver mapped an arbitrary physical address" stops
/// being a review question.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct Device {
    span: Span,
}

impl Device {
    pub const fn span(self) -> Span {
        self.span
    }

    /// Checks a requested mapping of this window and reports its policy.
    ///
    /// The cache policy is not the caller's to choose, which is the point:
    /// there is exactly one correct answer for MMIO and it is returned rather
    /// than requested.
    pub const fn mapping(self, rights: Rights) -> Result<(Rights, Cache), MappingError> {
        match Kind::Device.allows(rights, Cache::Device) {
            Ok(()) => Ok((rights, Cache::Device)),
            Err(error) => Err(error),
        }
    }
}
