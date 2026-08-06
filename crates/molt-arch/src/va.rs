//! The one virtual address space every tier shares: which part of it a mapping
//! gets, and when a freed part may be handed out again.
//!
//! Molt has no per-process address space, so a virtual address means the same
//! thing on every hart and in every domain. That makes the address space a
//! global resource with exactly one allocator, and this is it. [`Space`] cuts
//! the addresses this kernel hands out into one [`Arena`](Class) per leaf size,
//! so an extent that will be mapped with gigabyte leaves is aligned to a
//! gigabyte without a search, and hands ranges out of each arena
//! address-ordered first fit with immediate coalescing — the policy
//! `molt-alloc` already uses for the heap, for the reasons written down in
//! `docs/va-allocator.md`.
//!
//! A freed address is not a free address: while some hart may still hold a TLB
//! entry for it, handing it to someone else would let that hart read the new
//! owner's memory. So [`release`](Space::release) stamps the range with the
//! open shootdown [`Epoch`], and only [`retire`](Space::retire) — called once
//! every hart has flushed — makes it allocatable again.

use crate::FRAME_SIZE;

/// Why a request against the address space was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The address width is too narrow to cut into one arena per class.
    Width,
    /// Fewer hole slots than the one per class the space starts out holding.
    Storage,
    /// A request for zero bytes, which names no page.
    Empty,
    /// No free extent of this class is large enough.
    Exhausted,
    /// The class has no slot left to record the freed extent in.
    Full,
    /// The extent did not come from this space, or not from this class.
    Foreign,
    /// The extent overlaps a range this space already holds free, which means
    /// it was released twice.
    Overlap,
}

/// The leaf size an extent will be mapped with, and so the alignment it needs.
///
/// One class per page-table level: a mapping that wants gigabyte leaves has to
/// start on a gigabyte boundary, and asking for that after the fact is a search
/// through a fragmented space. Asking for it up front is a class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Class {
    /// 4 KiB leaves: the level every architecture has.
    Page,
    /// 2 MiB leaves.
    Mega,
    /// 1 GiB leaves: what a hundred-gigabyte mapping is made of.
    Giga,
}

impl Class {
    /// Every class, smallest granule first.
    pub const ALL: [Self; 3] = [Self::Page, Self::Mega, Self::Giga];

    /// How many leaves of the class below fit in one leaf of a class: the
    /// entries in a page table, on every architecture molt maps with.
    pub const FANOUT: u64 = 512;

    /// The leaf size, one page-table level apart from the next.
    pub const fn granule(self) -> u64 {
        FRAME_SIZE << (9 * self.level())
    }

    /// The class one page-table level down, or `None` at the leaves.
    ///
    /// This is what revoking part of a gigabyte costs: the leaf that covers it
    /// has to become [`FANOUT`](Self::FANOUT) leaves of this class before any
    /// of them can be treated separately.
    pub const fn smaller(self) -> Option<Self> {
        match self {
            Self::Page => None,
            Self::Mega => Some(Self::Page),
            Self::Giga => Some(Self::Mega),
        }
    }

    /// The class one page-table level up, or `None` at the largest leaf molt
    /// maps with.
    pub const fn larger(self) -> Option<Self> {
        match self {
            Self::Page => Some(Self::Mega),
            Self::Mega => Some(Self::Giga),
            Self::Giga => None,
        }
    }

    /// The page-table level the leaf sits at, counting from the leaves.
    pub const fn level(self) -> u32 {
        match self {
            Self::Page => 0,
            Self::Mega => 1,
            Self::Giga => 2,
        }
    }

    const fn index(self) -> usize {
        self.level() as usize
    }
}

/// What one machine's translation hardware turned out to be able to do.
///
/// Both numbers are probed rather than assumed — a hart reports its widest
/// `satp` mode, an x86-64 core its `CR4.LA57` and its PCID support — because
/// both decide how much of the design is available: the address width is what
/// [`Space::over`] cuts, and the tag width is how many domains can hold a
/// translation at once (see [`Asids`](crate::asid::Asids)).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Widths {
    address: u32,
    asid: u32,
}

impl Widths {
    pub const fn new(address: u32, asid: u32) -> Self {
        Self { address, asid }
    }

    /// How many virtual address bits translation resolves.
    pub const fn address(self) -> u32 {
        self.address
    }

    /// How many tag bits keep two views of one address apart. Zero is a real
    /// answer: it means every view switch is a flush.
    pub const fn asid(self) -> u32 {
        self.asid
    }
}

/// A shootdown generation.
///
/// A freed range carries the epoch whose flush has to finish before its
/// addresses may be handed out again, so the allocator can answer "is this
/// address safe to reuse" by comparing two numbers.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Epoch(u64);

impl Epoch {
    /// Before anything has been freed, so nothing waits on a flush.
    pub const FIRST: Self = Self(0);

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A half-open range of virtual addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Region {
    start: u64,
    end: u64,
}

impl Region {
    /// Rejects empty and inverted bounds.
    pub const fn new(start: u64, end: u64) -> Result<Self, Error> {
        if start >= end {
            return Err(Error::Empty);
        }
        Ok(Self { start, end })
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

    pub const fn contains(self, address: u64) -> bool {
        self.start <= address && address < self.end
    }

    pub const fn covers(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// A range of virtual addresses one mapping owns.
///
/// Non-copy for the same reason [`Frames`](crate::memory::Frames) is: the space
/// it came from is the only thing that can take it back, and an extent that is
/// dropped instead is an address range nobody will ever hand out again.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "an extent leaks its addresses unless it is released or stored"]
pub struct Extent {
    region: Region,
    class: Class,
}

impl Extent {
    pub const fn region(&self) -> Region {
        self.region
    }

    pub const fn class(&self) -> Class {
        self.class
    }

    pub const fn start(&self) -> u64 {
        self.region.start
    }

    pub const fn end(&self) -> u64 {
        self.region.end
    }

    pub const fn bytes(&self) -> u64 {
        self.region.bytes()
    }

    /// How many leaves of this extent's class it takes to map it.
    ///
    /// This is also how many refcounts a grant of the whole extent touches:
    /// molt counts leaves, not frames, so a gigabyte-class extent costs one
    /// count per gigabyte rather than 262 144 per gigabyte.
    pub const fn leaves(&self) -> u64 {
        self.region.bytes() / self.class.granule()
    }
}

/// One free range, and the epoch it becomes allocatable in.
///
/// This is the allocator's storage cell: a caller supplies a slice of these,
/// the way [`FrameTable`](crate::memory::FrameTable) is handed its slots, so
/// the address space allocator needs no allocator of its own.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Hole {
    start: u64,
    end: u64,
    ready: Epoch,
}

impl Hole {
    /// An unused slot, so a caller can write `[Hole::EMPTY; 64]`.
    pub const EMPTY: Self = Self { start: 0, end: 0, ready: Epoch::FIRST };

    /// The range this hole covers, or `None` for an unused slot.
    pub const fn region(self) -> Option<Region> {
        match Region::new(self.start, self.end) {
            Ok(region) => Some(region),
            Err(_) => None,
        }
    }

    /// The epoch that has to retire before this hole may be handed out.
    pub const fn ready(self) -> Epoch {
        self.ready
    }

    const fn bytes(self) -> u64 {
        self.end - self.start
    }
}

/// The free list of one class, over one range of the address space.
struct Arena<'holes> {
    class: Class,
    bounds: Region,
    holes: &'holes mut [Hole],
    len: usize,
}

impl<'holes> Arena<'holes> {
    fn new(class: Class, bounds: Region, holes: &'holes mut [Hole]) -> Result<Self, Error> {
        if holes.is_empty() {
            return Err(Error::Storage);
        }
        if bounds.bytes() < class.granule() || bounds.start % class.granule() != 0 {
            return Err(Error::Width);
        }
        holes.fill(Hole::EMPTY);
        holes[0] = Hole { start: bounds.start, end: bounds.end, ready: Epoch::FIRST };
        Ok(Self { class, bounds, holes, len: 1 })
    }

    /// Lowest free range of at least `bytes`, rounded up to the class granule.
    ///
    /// Every arena bound and every carve is a multiple of the granule, so the
    /// alignment the class promises is an invariant of the free list rather
    /// than something the search has to look for.
    fn allocate(&mut self, bytes: u64, retired: Epoch) -> Result<Extent, Error> {
        let size = bytes.checked_next_multiple_of(self.class.granule()).ok_or(Error::Exhausted)?;
        let index = (0..self.len)
            .find(|&index| self.holes[index].ready <= retired && self.holes[index].bytes() >= size)
            .ok_or(Error::Exhausted)?;

        let hole = &mut self.holes[index];
        let start = hole.start;
        hole.start += size;
        if hole.start == hole.end {
            self.remove(index);
        }
        Ok(Extent { region: Region { start, end: start + size }, class: self.class })
    }

    /// Puts a range back, stamped with the epoch a flush has to cover.
    ///
    /// Only neighbours waiting on the same epoch are coalesced. Merging a
    /// quarantined range into a free one would have to take the later epoch of
    /// the two, and one freed gigabyte would put the whole rest of the arena
    /// behind the next flush; merging the other way would hand out addresses a
    /// hart may still have cached. Ranges freed in one batch do join each
    /// other, and [`settle`](Self::settle) merges the batch into its
    /// neighbours once the flush retires.
    fn release(&mut self, extent: Extent, ready: Epoch) -> Result<(), Error> {
        if extent.class != self.class || !self.bounds.covers(extent.region) {
            return Err(Error::Foreign);
        }
        let (start, end) = (extent.region.start, extent.region.end);

        let at = (0..self.len).find(|&index| self.holes[index].start >= end).unwrap_or(self.len);
        // Everything below `at` ends at or before `end`; only the range just
        // below it can still reach into what is being freed.
        if at > 0 && self.holes[at - 1].end > start {
            return Err(Error::Overlap);
        }

        let joins_below =
            at > 0 && self.holes[at - 1].end == start && self.holes[at - 1].ready == ready;
        let joins_above =
            at < self.len && self.holes[at].start == end && self.holes[at].ready == ready;
        match (joins_below, joins_above) {
            (true, true) => {
                self.holes[at - 1].end = self.holes[at].end;
                self.remove(at);
            }
            (true, false) => self.holes[at - 1].end = end,
            (false, true) => self.holes[at].start = start,
            (false, false) => {
                if self.len == self.holes.len() {
                    return Err(Error::Full);
                }
                self.holes.copy_within(at..self.len, at + 1);
                self.holes[at] = Hole { start, end, ready };
                self.len += 1;
            }
        }
        Ok(())
    }

    /// Merges the neighbours a retired flush has just made interchangeable.
    ///
    /// This is where the free list gets its slots back: a batch of releases can
    /// leave an island per group of adjacent ranges, and once every hart has
    /// flushed there is nothing left to tell those islands apart from the free
    /// space around them.
    fn settle(&mut self, retired: Epoch) {
        let mut index = 0;
        while index + 1 < self.len {
            let (hole, next) = (self.holes[index], self.holes[index + 1]);
            if hole.end == next.start && hole.ready <= retired && next.ready <= retired {
                self.holes[index].end = next.end;
                self.holes[index].ready = hole.ready.max(next.ready);
                self.remove(index + 1);
                continue;
            }
            index += 1;
        }
    }

    fn remove(&mut self, index: usize) {
        self.holes.copy_within(index + 1..self.len, index);
        self.len -= 1;
        self.holes[self.len] = Hole::EMPTY;
    }

    /// Bytes that can be handed out right now.
    fn free(&self, retired: Epoch) -> u64 {
        self.holes[..self.len]
            .iter()
            .filter(|hole| hole.ready <= retired)
            .map(|hole| hole.bytes())
            .sum()
    }

    /// Bytes that are free but still waiting on a flush.
    fn quarantined(&self, retired: Epoch) -> u64 {
        self.holes[..self.len]
            .iter()
            .filter(|hole| hole.ready > retired)
            .map(|hole| hole.bytes())
            .sum()
    }

    /// The largest single extent this arena could hand out right now, which is
    /// what fragmentation actually costs a caller.
    fn largest(&self, retired: Epoch) -> u64 {
        self.holes[..self.len]
            .iter()
            .filter(|hole| hole.ready <= retired)
            .map(|hole| hole.bytes())
            .max()
            .unwrap_or(0)
    }
}

/// The virtual addresses this kernel hands out, one arena per leaf size.
pub struct Space<'holes> {
    arenas: [Arena<'holes>; Class::ALL.len()],
    open: Epoch,
    retired: Epoch,
}

impl<'holes> Space<'holes> {
    /// Cuts the range [`bounds`](Self::bounds) names into one arena per class
    /// and splits `holes` evenly between their free lists.
    ///
    /// The gigabyte class gets half the space and the other two a quarter each:
    /// the classes cannot borrow from one another, so the one whose extents are
    /// largest gets the most room. Sizing is by ratio rather than by absolute
    /// numbers because the width the hart reports decides how much there is.
    pub fn over(bits: u32, holes: &'holes mut [Hole]) -> Result<Self, Error> {
        let bounds = Self::bounds(bits)?;
        if holes.len() < Class::ALL.len() {
            return Err(Error::Storage);
        }

        let each = holes.len() / Class::ALL.len();
        let (page, rest) = holes.split_at_mut(each);
        let (mega, giga) = rest.split_at_mut(each);
        let quarter = bounds.bytes() / 4;
        let first = bounds.start();

        Ok(Self {
            arenas: [
                Arena::new(Class::Page, Region::new(first, first + quarter)?, page)?,
                Arena::new(Class::Mega, Region::new(first + quarter, first + 2 * quarter)?, mega)?,
                Arena::new(Class::Giga, Region::new(first + 2 * quarter, bounds.end())?, giga)?,
            ],
            // Nothing has been freed yet, so the first batch of releases is
            // already open and no flush is outstanding.
            open: Epoch::FIRST.next(),
            retired: Epoch::FIRST,
        })
    }

    /// The top quarter of the lower canonical half of a `bits`-wide space.
    ///
    /// Everything below stays with the kernel: that is where the identity map
    /// of RAM lives, and on RISC-V it is also where the device window sits.
    /// The narrowest mode is the tight one — Sv39 puts this range at
    /// [192 GiB, 256 GiB), clear of `paging::DEVICE_REGION` at 128 GiB — and a
    /// wider mode moves the same fraction further out without changing
    /// anything else about the layout.
    ///
    /// A width below 35 bits cannot be cut into four arenas of a gigabyte, so
    /// it is refused rather than handing out extents no gigabyte leaf fits in.
    pub const fn bounds(bits: u32) -> Result<Region, Error> {
        if bits < 35 || bits > 64 {
            return Err(Error::Width);
        }
        let quarter = 1u64 << (bits - 3);
        Region::new(3 * quarter, 4 * quarter)
    }

    /// Takes a range of at least `bytes`, aligned to the class granule.
    pub fn allocate(&mut self, class: Class, bytes: u64) -> Result<Extent, Error> {
        if bytes == 0 {
            return Err(Error::Empty);
        }
        let retired = self.retired;
        self.arenas[class.index()].allocate(bytes, retired)
    }

    /// Gives an extent back, into the open shootdown batch.
    ///
    /// The addresses stay out of circulation until that batch is
    /// [`sweep`](Self::sweep)ed and the flush it names is
    /// [`retire`](Self::retire)d, because until then a hart may still hold a
    /// translation for them.
    pub fn release(&mut self, extent: Extent) -> Result<(), Error> {
        let open = self.open;
        self.arenas[extent.class.index()].release(extent, open)
    }

    /// Closes the batch of released extents and names the epoch a shootdown has
    /// to cover before any of them can be reused.
    pub fn sweep(&mut self) -> Epoch {
        let closing = self.open;
        self.open = closing.next();
        closing
    }

    /// Records that every hart has flushed through `epoch`.
    ///
    /// An epoch that was never swept is ignored: the batch it names is still
    /// taking releases, so nothing can have flushed it.
    pub fn retire(&mut self, epoch: Epoch) {
        if epoch >= self.open {
            return;
        }
        self.retired = self.retired.max(epoch);
        let retired = self.retired;
        for arena in &mut self.arenas {
            arena.settle(retired);
        }
    }

    /// The batch releases currently join.
    pub const fn open(&self) -> Epoch {
        self.open
    }

    /// The last epoch every hart has flushed.
    pub const fn retired(&self) -> Epoch {
        self.retired
    }

    /// The whole range one class hands out of.
    pub fn arena(&self, class: Class) -> Region {
        self.arenas[class.index()].bounds
    }

    /// Bytes of this class that can be handed out right now.
    pub fn free(&self, class: Class) -> u64 {
        self.arenas[class.index()].free(self.retired)
    }

    /// Bytes of this class that are free but still waiting on a flush.
    pub fn quarantined(&self, class: Class) -> u64 {
        self.arenas[class.index()].quarantined(self.retired)
    }

    /// The largest extent of this class that could be handed out right now.
    pub fn largest(&self, class: Class) -> u64 {
        self.arenas[class.index()].largest(self.retired)
    }

    /// How many separate free ranges this class is in, which is the
    /// fragmentation a run has caused.
    pub fn holes(&self, class: Class) -> usize {
        self.arenas[class.index()].len
    }
}
