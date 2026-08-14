//! The machine's one virtual address space: who gets which part of it, and when
//! a freed part may be handed out again. `docs/va-allocator.md` has the design.
//!
//! - One address space, so one allocator. An address means the same thing on
//!   every hart and in every domain.
//! - One arena per leaf size ([`Class`]), so a gigabyte-class extent comes back
//!   gigabyte-aligned without a search.
//! - Address-ordered first fit with immediate coalescing, the policy
//!   `molt-alloc` already uses for the heap.
//! - A freed address is not a free address: a hart may still hold a translation
//!   for it. [`release`](Space::release) stamps the range with the open
//!   [`Epoch`], and only [`retire`](Space::retire) — once every hart has
//!   flushed — makes it allocatable again.

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

impl From<(Error, Extent)> for Error {
    /// Drops the extent a refused [`release`](Space::release) handed back, so a
    /// caller that has nothing better to do with it can write `?`.
    fn from((error, _): (Error, Extent)) -> Self {
        error
    }
}

/// The leaf size an extent will be mapped with, and so the alignment it needs.
///
/// One class per page-table level. Asking for the alignment up front is a
/// class; asking after the fact is a search through a fragmented space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Class {
    /// 4 KiB leaves.
    Page,
    /// 2 MiB leaves.
    Mega,
    /// 1 GiB leaves.
    Giga,
}

impl Class {
    /// Every class, smallest granule first.
    pub const ALL: [Self; 3] = [Self::Page, Self::Mega, Self::Giga];

    /// Leaves of the class below per leaf of a class, which is the entry count
    /// of a page table on every architecture molt maps with.
    pub const FANOUT: u64 = 512;

    /// The leaf size, one page-table level apart from the next.
    pub const fn granule(self) -> u64 {
        FRAME_SIZE << (9 * self.level())
    }

    /// The class one page-table level down, or `None` at the leaves.
    ///
    /// Revoking part of a gigabyte costs this: the leaf covering it becomes
    /// [`FANOUT`](Self::FANOUT) leaves of the class below first.
    pub const fn smaller(self) -> Option<Self> {
        match self {
            Self::Page => None,
            Self::Mega => Some(Self::Page),
            Self::Giga => Some(Self::Mega),
        }
    }

    /// The class one page-table level up, or `None` at the largest leaf.
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

    /// The class whose leaves sit at page-table `level`, for a port walking its
    /// own tables, which names levels rather than classes.
    pub const fn at(level: u32) -> Option<Self> {
        match level {
            0 => Some(Self::Page),
            1 => Some(Self::Mega),
            2 => Some(Self::Giga),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        self.level() as usize
    }
}

/// What one machine's translation hardware turned out to be able to do.
///
/// Both numbers are probed, never assumed: the address width is what
/// [`Space::over`] cuts, and the tag width is how many domains can hold a
/// translation at once ([`Asids`](crate::asid::Asids)).
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

/// A shootdown generation: the flush a freed range waits on, so that "is this
/// address safe to reuse" is a comparison of two numbers.
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
/// Non-copy, like [`Frames`](crate::memory::Frames): only the space it came
/// from can take it back, and a dropped extent is addresses nobody can name.
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

    /// How many leaves of this extent's class it takes to map it, which is also
    /// how many counts a grant of the whole extent touches ([`refcount`]).
    ///
    /// [`refcount`]: crate::refcount
    pub const fn leaves(&self) -> u64 {
        self.region.bytes() / self.class.granule()
    }
}

/// One free range, and the epoch it becomes allocatable in.
///
/// The allocator's storage cell: a caller supplies a slice of these, the way
/// [`FrameTable`](crate::memory::FrameTable) is handed its slots, so the
/// address space needs no allocator of its own.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Hole {
    start: u64,
    end: u64,
    ready: Epoch,
}

impl Hole {
    /// An unused slot, so a caller can write `[Hole::EMPTY; 64]`.
    pub const EMPTY: Self = Self { start: 0, end: 0, ready: Epoch::FIRST };

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
    /// class alignment is an invariant of the free list rather than a search.
    fn allocate(&mut self, bytes: u64, retired: Epoch) -> Result<Extent, Error> {
        let size = bytes.checked_next_multiple_of(self.class.granule()).ok_or(Error::Exhausted)?;
        let index = self.holes[..self.len]
            .iter()
            .position(|hole| hole.ready <= retired && hole.bytes() >= size)
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
    /// Only neighbours waiting on the same epoch coalesce: merging a
    /// quarantined range into a free one takes the later of the two epochs, so
    /// one freed gigabyte would put the rest of the arena behind the next
    /// flush. [`settle`](Self::settle) joins the batch to its neighbours once
    /// the flush retires.
    ///
    /// A full free list is the exception, where the choice is between merging
    /// across epochs and having nowhere to put the range at all. A neighbour
    /// held back one flush is cheaper than addresses nobody can name again,
    /// which is also why a refusal hands the extent back.
    fn release(&mut self, extent: Extent, ready: Epoch) -> Result<(), (Error, Extent)> {
        if extent.class != self.class || !self.bounds.covers(extent.region) {
            return Err((Error::Foreign, extent));
        }
        let (start, end) = (extent.region.start, extent.region.end);

        // Sorted by start, so the first hole at or above `end` is a bisection.
        let at = self.holes[..self.len].partition_point(|hole| hole.start < end);
        // Everything below `at` ends at or before `end`; only the range just
        // below it can still reach into what is being freed.
        if at > 0 && self.holes[at - 1].end > start {
            return Err((Error::Overlap, extent));
        }

        let full = self.len == self.holes.len();
        let joins = |hole: Hole| full || hole.ready == ready;
        let below = at > 0 && self.holes[at - 1].end == start && joins(self.holes[at - 1]);
        let above = at < self.len && self.holes[at].start == end && joins(self.holes[at]);

        // `ready` is the open epoch, so it is the latest of whatever merges.
        match (below, above) {
            (true, true) => {
                self.holes[at - 1] = Hole { end: self.holes[at].end, ready, ..self.holes[at - 1] };
                self.remove(at);
            }
            (true, false) => self.holes[at - 1] = Hole { end, ready, ..self.holes[at - 1] },
            (false, true) => self.holes[at] = Hole { start, ready, ..self.holes[at] },
            (false, false) if full => return Err((Error::Full, extent)),
            (false, false) => {
                self.holes.copy_within(at..self.len, at + 1);
                self.holes[at] = Hole { start, end, ready };
                self.len += 1;
            }
        }
        Ok(())
    }

    /// Merges the neighbours a retired flush has made interchangeable, which is
    /// where the free list gets its slots back.
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

    /// The largest single extent this arena could hand out, which is what
    /// fragmentation costs a caller.
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
    /// Half the space to the gigabyte class and a quarter to each of the other
    /// two: classes cannot borrow from one another, so the largest extents get
    /// the most room. By ratio, because the probed width decides the total.
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
    /// Everything below stays with the kernel: the identity map of RAM, and on
    /// RISC-V the device window too. Sv39 is the tight case — [192 GiB,
    /// 256 GiB), clear of `paging::DEVICE_REGION` at 128 GiB — and a wider mode
    /// moves the same fraction further out. Below 35 bits the quarters are
    /// under a gigabyte, so no gigabyte leaf would fit and the width is refused.
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
    /// [`sweep`](Self::sweep)ed and its flush [`retire`](Self::retire)d.
    ///
    /// A refusal hands the extent back, since nothing else can name the range.
    /// [`Error::Full`] is the recoverable one: the next release that joins two
    /// free ranges leaves a slot to record this one in.
    pub fn release(&mut self, extent: Extent) -> Result<(), (Error, Extent)> {
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

    /// Records that every hart has flushed through `epoch`. An unswept epoch is
    /// ignored: its batch is still taking releases, so nothing can have flushed
    /// it.
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

    /// How many separate free ranges this class is in: its fragmentation.
    pub fn holes(&self, class: Class) -> usize {
        self.arenas[class.index()].len
    }
}
