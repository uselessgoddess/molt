//! Who else still has this mapping, counted once per leaf.
//!
//! A shared extent needs a count somewhere: the last view to give a gigabyte
//! back is the one that may unmap it, and nothing below this module knows which
//! view that was. The question is what the count is keyed on.
//!
//! Keying it on the frame is what a kernel with 4 KiB pages does, and it is the
//! wrong unit here. A gigabyte leaf mapped into two views would need 262 144
//! per-frame records to say the number two, all of them saying it, none of them
//! ever consulted on their own — the leaf is what was mapped, so the leaf is
//! what a hart can hold a translation for, and the leaf is what gets unmapped.
//! So the count lives on the leaf, and a gigabyte costs one.
//!
//! One leaf is still not the unit of storage, because a hundred-gigabyte extent
//! is a hundred leaves that were mapped together and will be shared together.
//! [`Leaves`] stores a [`Run`] per stretch of adjacent same-class leaves that
//! agree on their count, so the common case — map a lot, share all of it — is
//! one record however large it is, and records appear only where views actually
//! disagree.
//!
//! What that buys is paid for at the edges. A count that covers a gigabyte
//! cannot say anything about two megabytes inside it, so a range that ends
//! inside a leaf is refused ([`Error::Straddle`]) until [`split`](Leaves::split)
//! turns that leaf into [`Class::FANOUT`] of the next class down — which is
//! exactly what the page tables have to do anyway before that subrange can be
//! revoked. [`merge`](Leaves::merge) is the way back once the counts agree
//! again.

use crate::FRAME_SIZE;
use crate::va::{Class, Region};

/// Why an accounting request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// No record slot left to describe the leaves the request would create.
    Storage,
    /// The address is not on a boundary of the class it names.
    Misaligned,
    /// A leaf covering the range is already counted.
    Overlap,
    /// Part of the range is not counted at all, so there is nothing to add to.
    Untracked,
    /// The range ends inside a leaf. Split that leaf before treating its parts
    /// differently.
    Straddle,
    /// There is no page-table level below `Page` to split into, or none above
    /// `Giga` to merge into.
    Granule,
    /// The leaves to merge are not one whole aligned group with one count.
    Uneven,
    /// The range wraps the top of the address space.
    Address,
    /// One more reference than a count can hold.
    Saturated,
    /// The range is counted, but nobody claims to hold it.
    Unreferenced,
}

/// A stretch of adjacent leaves of one class that share one count.
///
/// The stretch is the storage unit, not the leaf: a caller supplies a slice of
/// these the way [`Space`](crate::va::Space) is handed its holes, so counting a
/// hundred gigabytes needs no allocator and no per-frame table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Run {
    start: u64,
    leaves: u64,
    class: Class,
    count: u32,
}

impl Run {
    /// An unused slot, so a caller can write `[Run::EMPTY; 16]`.
    pub const EMPTY: Self = Self { start: 0, leaves: 0, class: Class::Page, count: 0 };

    /// The addresses these leaves cover, or `None` for an unused slot.
    pub const fn region(self) -> Option<Region> {
        match Region::new(self.start, self.start + self.leaves * self.class.granule()) {
            Ok(region) => Some(region),
            Err(_) => None,
        }
    }

    /// The leaf size every leaf in the run was mapped with.
    pub const fn class(self) -> Class {
        self.class
    }

    /// How many leaves the run holds.
    pub const fn leaves(self) -> u64 {
        self.leaves
    }

    /// How many views hold each of them.
    pub const fn count(self) -> u32 {
        self.count
    }

    const fn end(self) -> u64 {
        self.start + self.leaves * self.class.granule()
    }

    const fn holds(self, address: u64) -> bool {
        self.leaves != 0 && self.start <= address && address < self.end()
    }
}

/// What a [`release`](Leaves::release) left behind: leaves nobody holds any
/// more, and so mappings the caller may now take down.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reclaimed {
    leaves: u64,
    bytes: u64,
}

impl Reclaimed {
    /// How many leaves reached a count of zero.
    pub const fn leaves(self) -> u64 {
        self.leaves
    }

    /// How much address space they covered.
    pub const fn bytes(self) -> u64 {
        self.bytes
    }

    /// Whether the range is still held by somebody.
    pub const fn is_empty(self) -> bool {
        self.leaves == 0
    }
}

/// The counts this kernel keeps, keyed on the leaves it actually mapped.
pub struct Leaves<'runs> {
    runs: &'runs mut [Run],
    len: usize,
}

impl<'runs> Leaves<'runs> {
    /// Takes the slice the counts live in. Nothing is counted yet.
    pub fn over(runs: &'runs mut [Run]) -> Self {
        runs.fill(Run::EMPTY);
        Self { runs, len: 0 }
    }

    /// Starts counting `leaves` leaves of `class` at `start`, held once.
    ///
    /// This is what a fresh mapping costs to account for: one record, whatever
    /// the size, unless it sits next to leaves of the same class that are also
    /// held once — in which case it costs nothing at all.
    pub fn map(&mut self, start: u64, class: Class, leaves: u64) -> Result<(), Error> {
        let run = Self::run(start, class, leaves, 1)?;
        let at = self.place(run)?;
        self.insert(at, run)?;
        self.coalesce();
        Ok(())
    }

    /// Adds a reference to every leaf covering `region`.
    ///
    /// The range has to be whole leaves: a grant of two megabytes out of a
    /// gigabyte leaf is [`Error::Straddle`] until the leaf is
    /// [`split`](Self::split), because the page tables cannot hand out half a
    /// translation either.
    pub fn share(&mut self, region: Region) -> Result<(), Error> {
        let (first, last) = self.cover(region)?;
        for run in &mut self.runs[first..last] {
            run.count = run.count.checked_add(1).ok_or(Error::Saturated)?;
        }
        self.coalesce();
        Ok(())
    }

    /// Drops a reference from every leaf covering `region`, and reports the
    /// leaves that reached zero.
    ///
    /// A leaf nobody holds is dropped from the table: the caller unmaps it,
    /// shoots the address down, and only then may the address be handed out
    /// again — none of which this module can do, which is why it says so
    /// instead.
    pub fn release(&mut self, region: Region) -> Result<Reclaimed, Error> {
        let (first, last) = self.cover(region)?;
        for run in &mut self.runs[first..last] {
            run.count = run.count.checked_sub(1).ok_or(Error::Unreferenced)?;
        }

        let mut reclaimed = Reclaimed::default();
        let mut index = first;
        let mut end = last;
        while index < end {
            if self.runs[index].count != 0 {
                index += 1;
                continue;
            }
            let run = self.runs[index];
            reclaimed.leaves += run.leaves;
            reclaimed.bytes += run.leaves * run.class.granule();
            self.remove(index);
            end -= 1;
        }
        self.coalesce();
        Ok(reclaimed)
    }

    /// Turns the leaf covering `address` into [`Class::FANOUT`] leaves of the
    /// class below, each still held by everyone who held the leaf.
    ///
    /// Splitting changes what can be said, not what is true: the same addresses
    /// are held by the same views before and after, which is why the counts are
    /// copied down rather than divided. It is the page-table operation that has
    /// to precede revoking part of a large leaf, mirrored in the accounting.
    pub fn split(&mut self, address: u64) -> Result<Class, Error> {
        let index = self.find(address).ok_or(Error::Untracked)?;
        let run = self.runs[index];
        let child = run.class.smaller().ok_or(Error::Granule)?;
        let leaf = address - (address - run.start) % run.class.granule();

        self.cut(leaf)?;
        self.cut(leaf + run.class.granule())?;
        let index = self.find(leaf).ok_or(Error::Untracked)?;
        self.runs[index] = Self::run(leaf, child, Class::FANOUT, run.count)?;
        self.coalesce();
        Ok(child)
    }

    /// Puts one aligned group of [`Class::FANOUT`] leaves back together, if
    /// they all still agree on their count.
    ///
    /// Disagreement is the whole reason the group was split, so a group that
    /// still disagrees is [`Error::Uneven`] rather than a count invented to
    /// cover both.
    pub fn merge(&mut self, address: u64) -> Result<Class, Error> {
        let index = self.find(address).ok_or(Error::Untracked)?;
        let run = self.runs[index];
        let parent = run.class.larger().ok_or(Error::Granule)?;
        let group = address - address % parent.granule();
        let region = Region::new(group, group + parent.granule()).map_err(|_| Error::Address)?;

        let (first, last) = self.cover(region)?;
        if last != first + 1 || self.runs[first].class.larger() != Some(parent) {
            return Err(Error::Uneven);
        }
        let count = self.runs[first].count;
        self.runs[first] = Self::run(group, parent, 1, count)?;
        self.coalesce();
        Ok(parent)
    }

    /// How many views hold the leaf covering `address`, if it is counted.
    pub fn count(&self, address: u64) -> Option<u32> {
        self.find(address).map(|index| self.runs[index].count)
    }

    /// The class of the leaf covering `address`, if it is counted.
    pub fn class(&self, address: u64) -> Option<Class> {
        self.find(address).map(|index| self.runs[index].class)
    }

    /// The records in use, which is what the accounting actually costs.
    pub const fn runs(&self) -> usize {
        self.len
    }

    /// Every record in use, lowest address first.
    pub fn iter(&self) -> impl Iterator<Item = Run> {
        self.runs[..self.len].iter().copied()
    }

    /// How many leaves are counted.
    pub fn leaves(&self) -> u64 {
        self.runs[..self.len].iter().map(|run| run.leaves).sum()
    }

    /// How much address space they cover.
    pub fn bytes(&self) -> u64 {
        self.runs[..self.len].iter().map(|run| run.leaves * run.class.granule()).sum()
    }

    /// How many frames that is — the number of records a per-frame table would
    /// have needed to say the same thing.
    pub fn frames(&self) -> u64 {
        self.bytes() / FRAME_SIZE
    }

    fn run(start: u64, class: Class, leaves: u64, count: u32) -> Result<Run, Error> {
        if leaves == 0 {
            return Err(Error::Untracked);
        }
        if start % class.granule() != 0 {
            return Err(Error::Misaligned);
        }
        leaves
            .checked_mul(class.granule())
            .and_then(|bytes| start.checked_add(bytes))
            .ok_or(Error::Address)?;
        Ok(Run { start, leaves, class, count })
    }

    /// Where a new run belongs, refusing one that overlaps a counted leaf.
    fn place(&self, run: Run) -> Result<usize, Error> {
        let at =
            (0..self.len).find(|&index| self.runs[index].start >= run.start).unwrap_or(self.len);
        if at > 0 && self.runs[at - 1].end() > run.start {
            return Err(Error::Overlap);
        }
        if at < self.len && self.runs[at].start < run.end() {
            return Err(Error::Overlap);
        }
        Ok(at)
    }

    /// The half-open range of records that covers `region` exactly, after
    /// cutting records at both of its edges.
    fn cover(&mut self, region: Region) -> Result<(usize, usize), Error> {
        self.cut(region.start())?;
        self.cut(region.end())?;

        let first = self.find(region.start()).ok_or(Error::Untracked)?;
        let mut last = first;
        let mut reached = self.runs[first].start;
        while last < self.len && self.runs[last].start == reached {
            reached = self.runs[last].end();
            last += 1;
            if reached >= region.end() {
                break;
            }
        }
        if reached < region.end() {
            return Err(Error::Untracked);
        }
        Ok((first, last))
    }

    /// Makes `address` a record boundary, so a request can stop there.
    fn cut(&mut self, address: u64) -> Result<(), Error> {
        let Some(index) = self.find(address) else {
            return Ok(());
        };
        let run = self.runs[index];
        if run.start == address {
            return Ok(());
        }
        if address % run.class.granule() != 0 {
            return Err(Error::Straddle);
        }
        // Refuse before touching anything: a record shortened without its other
        // half being stored is leaves nobody counts any more.
        if self.len == self.runs.len() {
            return Err(Error::Storage);
        }
        let before = (address - run.start) / run.class.granule();
        self.runs[index].leaves = before;
        self.insert(index + 1, Run { start: address, leaves: run.leaves - before, ..run })
    }

    fn find(&self, address: u64) -> Option<usize> {
        (0..self.len).find(|&index| self.runs[index].holds(address))
    }

    fn insert(&mut self, at: usize, run: Run) -> Result<(), Error> {
        if self.len == self.runs.len() {
            return Err(Error::Storage);
        }
        self.runs.copy_within(at..self.len, at + 1);
        self.runs[at] = run;
        self.len += 1;
        Ok(())
    }

    fn remove(&mut self, at: usize) {
        self.runs.copy_within(at + 1..self.len, at);
        self.len -= 1;
        self.runs[self.len] = Run::EMPTY;
    }

    /// Joins neighbours that have nothing left to disagree about.
    ///
    /// Records exist to hold a count, so two adjacent stretches of one class
    /// with one count are one record's worth of information stored twice. This
    /// is what keeps a shared hundred-gigabyte extent at a single record after
    /// the cuts a grant of the whole thing goes through.
    fn coalesce(&mut self) {
        let mut index = 0;
        while index + 1 < self.len {
            let (run, next) = (self.runs[index], self.runs[index + 1]);
            let joins =
                run.end() == next.start && run.class == next.class && run.count == next.count;
            if joins {
                self.runs[index].leaves += next.leaves;
                self.remove(index + 1);
                continue;
            }
            index += 1;
        }
    }
}
