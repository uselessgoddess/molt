//! Who else still has this mapping, counted once per leaf.
//!
//! - **Keyed on the leaf, not the frame.** The leaf is what was mapped, what a
//!   hart caches a translation for, and what gets unmapped. Per-frame records
//!   would spend 262 144 of them saying "two" about one shared gigabyte.
//! - **Stored per run, not per leaf.** [`Leaves`] holds one [`Run`] per stretch
//!   of adjacent same-class leaves that agree on their count, so a hundred
//!   gigabytes mapped and shared together is one record. Records appear only
//!   where views disagree.
//! - **Paid for at the edges.** A count covering a gigabyte says nothing about
//!   two megabytes inside it, so such a range is [`Error::Straddle`] until
//!   [`split`](Leaves::split) cuts the leaf into [`Class::FANOUT`] of the class
//!   below — which the page tables owe anyway before that subrange can be
//!   revoked. [`merge`](Leaves::merge) is the way back.

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
}

/// A stretch of adjacent leaves of one class that share one count.
///
/// A caller supplies a slice of these, the way [`Space`](crate::va::Space) is
/// handed its holes, so counting a hundred gigabytes needs no allocator.
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
    pub fn region(self) -> Option<Region> {
        Region::new(self.start, self.end()).ok()
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

/// What a [`release`](Leaves::release) left behind: leaves nobody holds, and so
/// mappings the caller may now take down.
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
///
/// Records are sorted by address and never overlap — `place` refuses an
/// overlap, `insert` keeps the order — which is what lets every lookup bisect
/// instead of walking the table.
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
    /// One record whatever the size, or none at all when it joins neighbours of
    /// the same class also held once.
    pub fn map(&mut self, start: u64, class: Class, leaves: u64) -> Result<(), Error> {
        let run = Self::run(start, class, leaves, 1)?;
        let at = self.place(run)?;
        self.insert(at, run)?;
        self.coalesce();
        Ok(())
    }

    /// Adds a reference to every leaf covering `region`, which has to be whole
    /// leaves: half a translation is [`Error::Straddle`] until
    /// [`split`](Self::split).
    pub fn share(&mut self, region: Region) -> Result<(), Error> {
        // Asked before counting: a grant that stopped at the record it could
        // not count would leave references the caller was told it did not get.
        let saturated = self.runs[..self.len].iter().any(|run| {
            run.count == u32::MAX && run.start < region.end() && run.end() > region.start()
        });
        if saturated {
            return Err(Error::Saturated);
        }

        let (first, last) = self.cover(region)?;
        for run in &mut self.runs[first..last] {
            run.count += 1;
        }
        self.coalesce();
        Ok(())
    }

    /// Drops a reference from every leaf covering `region`, and reports the
    /// leaves that reached zero.
    ///
    /// Those leave the table, and the caller owes them an unmap, a shootdown
    /// and a retire, in that order — none of which this module can do.
    pub fn release(&mut self, region: Region) -> Result<Reclaimed, Error> {
        let (first, last) = self.cover(region)?;
        for run in &mut self.runs[first..last] {
            // Cannot underflow: `map` starts at one and the loop below drops
            // every record that reaches zero.
            run.count -= 1;
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
    /// Counts are copied down rather than divided: a split changes what can be
    /// said, not who holds what.
    pub fn split(&mut self, address: u64) -> Result<Class, Error> {
        let index = self.find(address).ok_or(Error::Untracked)?;
        let run = self.runs[index];
        let child = run.class.smaller().ok_or(Error::Granule)?;
        let leaf = address - address % run.class.granule();
        let region = Region::new(leaf, leaf + run.class.granule()).map_err(|_| Error::Address)?;

        let (index, _) = self.cover(region)?;
        self.runs[index] = Self::run(leaf, child, Class::FANOUT, run.count)?;
        self.coalesce();
        Ok(child)
    }

    /// Puts one aligned group of [`Class::FANOUT`] leaves back together, if they
    /// still agree on their count. A group that does not is [`Error::Uneven`],
    /// never a count invented to cover both.
    pub fn merge(&mut self, address: u64) -> Result<Class, Error> {
        let index = self.find(address).ok_or(Error::Untracked)?;
        let run = self.runs[index];
        let parent = run.class.larger().ok_or(Error::Granule)?;
        let group = address - address % parent.granule();
        let region = Region::new(group, group + parent.granule()).map_err(|_| Error::Address)?;
        // One record over the whole group is the group agreeing.
        if run.start > group || run.end() < region.end() {
            return Err(Error::Uneven);
        }

        let (first, _) = self.cover(region)?;
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

    /// How many frames that is: the records a per-frame table would have spent
    /// saying the same thing.
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
        let at = self.runs[..self.len].partition_point(|counted| counted.start < run.start);
        if at > 0 && self.runs[at - 1].end() > run.start {
            return Err(Error::Overlap);
        }
        if at < self.len && self.runs[at].start < run.end() {
            return Err(Error::Overlap);
        }
        Ok(at)
    }

    /// The half-open range of records covering `region` exactly, after cutting
    /// records at both edges.
    ///
    /// Every reason to refuse is found before the first cut: a request that
    /// stopped halfway would leave records split around something that never
    /// happened, spending the slots the next request needs.
    fn cover(&mut self, region: Region) -> Result<(usize, usize), Error> {
        // A region is never empty, so this asks about its own start first: a hole
        // anywhere under it is `Untracked` rather than a range that stops short.
        let mut reached = region.start();
        while reached < region.end() {
            reached = self.runs[self.find(reached).ok_or(Error::Untracked)?].end();
        }
        let cuts =
            usize::from(self.splits(region.start())?) + usize::from(self.splits(region.end())?);
        if self.len + cuts > self.runs.len() {
            return Err(Error::Storage);
        }

        for address in [region.start(), region.end()] {
            let Some(index) = self.find(address) else { continue };
            let run = self.runs[index];
            if run.start == address {
                continue;
            }
            let before = (address - run.start) / run.class.granule();
            self.runs[index].leaves = before;
            self.insert(index + 1, Run { start: address, leaves: run.leaves - before, ..run })?;
        }

        let first = self.find(region.start()).ok_or(Error::Untracked)?;
        let mut last = first;
        while self.runs[last].end() < region.end() {
            last += 1;
        }
        Ok((first, last + 1))
    }

    /// Whether a record has to be cut for a request to stop at `address`.
    fn splits(&self, address: u64) -> Result<bool, Error> {
        let Some(index) = self.find(address) else {
            return Ok(false);
        };
        let run = self.runs[index];
        if address % run.class.granule() != 0 {
            return Err(Error::Straddle);
        }
        Ok(run.start != address)
    }

    fn find(&self, address: u64) -> Option<usize> {
        // Records are sorted and disjoint, so the only one that can hold
        // `address` is the last starting at or below it: a bisection, asked
        // once per record a grant touches.
        let at = self.runs[..self.len].partition_point(|run| run.start <= address);
        at.checked_sub(1).filter(|&index| self.runs[index].holds(address))
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

    /// Joins neighbours that have nothing left to disagree about, which is what
    /// keeps a shared hundred-gigabyte extent at one record after a grant has
    /// cut it at both edges.
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
