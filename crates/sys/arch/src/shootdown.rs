//! Making an address safe to hand out again after it has been taken away.
//!
//! No store to a page table invalidates another core's cached translation, so
//! a revoke is three steps in an order that does not bend:
//!
//! 1. Clear the leaf.
//! 2. [`Tlb::flush`] on every core, delivered the way any other work reaches
//!    them.
//! 3. Only then [`retire`](crate::va::Space::retire) the addresses.
//!
//! Skip step 2 and the range still names the old frames on whichever core never
//! heard, so handing it to a second domain hands over the first one's memory.
//! [`Shootdown`] is the bookkeeping that gates step 3 on step 2: which cores
//! still owe a flush for which [`Epoch`].
//!
//! One round is open at a time. A core answers with its identity and nothing
//! else, so two rounds in flight cannot be told apart, and a round closed by
//! the wrong acknowledgement is the use-after-free this exists to prevent.
//! Batching happens the other side of it: one
//! [`sweep`](crate::va::Space::sweep) closes any number of releases into the
//! one epoch a round covers.

use crate::CpuId;
use crate::va::Epoch;

/// Why a shootdown step was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A core index the acknowledgement mask cannot name.
    Width,
    /// A round with no core in it, which would retire an epoch nobody flushed.
    Empty,
    /// A round is already open, and a second one cannot be told apart from it.
    Open,
    /// No round is open, so there is no flush to record.
    Closed,
    /// An epoch this tracker has already retired, or one older than that.
    Stale,
    /// A core the open round never asked.
    Foreign,
}

/// The cores a freed range is still waiting on, for one machine.
///
/// A round is a *set* of core indices rather than a count, so a core answering
/// twice cannot close a round the others are still in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shootdown {
    asked: u64,
    flushed: u64,
    /// The open round's epoch, and whether one is open: one field, because a
    /// closed tracker naming an epoch is a value every reader must distrust.
    epoch: Option<Epoch>,
    retired: Epoch,
    rounds: u64,
}

impl Default for Shootdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shootdown {
    /// Cores one acknowledgement mask can name.
    pub const LIMIT: u32 = u64::BITS;

    /// A tracker with nothing outstanding, which is where a boot starts.
    pub const fn new() -> Self {
        Self { asked: 0, flushed: 0, epoch: None, retired: Epoch::FIRST, rounds: 0 }
    }

    /// Opens a round: `epoch` stays unretired until every core in `cores` has
    /// flushed. Returns how many cores that is.
    ///
    /// The core doing the unmapping belongs in `cores` like any other — having
    /// just walked the tables, it is the likeliest to hold the translation.
    pub fn begin(
        &mut self,
        epoch: Epoch,
        mut cores: impl Iterator<Item = CpuId>,
    ) -> Result<u32, Error> {
        if self.epoch.is_some() {
            return Err(Error::Open);
        }
        if epoch <= self.retired {
            return Err(Error::Stale);
        }

        // Built to the side and installed whole: a half-filled round completes
        // before the rest of the cores have heard. The shift is what bounds the
        // index — a core the mask cannot name has no bit to set.
        let asked = cores.try_fold(0u64, |mask, cpu| {
            let bit = 1u64.checked_shl(cpu.index() as u32).ok_or(Error::Width)?;
            Ok(mask | bit)
        })?;
        if asked == 0 {
            return Err(Error::Empty);
        }

        self.asked = asked;
        self.flushed = 0;
        self.epoch = Some(epoch);
        Ok(asked.count_ones())
    }

    /// Records that `cpu` has flushed.
    ///
    /// Returns the epoch that became safe to retire, which is `Some` exactly
    /// once per round: on the acknowledgement that leaves nobody owing.
    pub fn acknowledge(&mut self, cpu: CpuId) -> Result<Option<Epoch>, Error> {
        let Some(epoch) = self.epoch else {
            return Err(Error::Closed);
        };
        if cpu.index() >= Self::LIMIT as usize || !self.in_round(cpu) {
            return Err(Error::Foreign);
        }

        self.flushed |= 1 << cpu.index();
        if self.flushed != self.asked {
            return Ok(None);
        }

        self.epoch = None;
        self.retired = epoch;
        self.rounds += 1;
        Ok(Some(epoch))
    }

    /// The epoch the open round covers, if a round is open.
    pub const fn epoch(&self) -> Option<Epoch> {
        self.epoch
    }

    /// How many cores still owe the open round a flush.
    pub const fn outstanding(&self) -> u32 {
        if self.epoch.is_some() { (self.asked & !self.flushed).count_ones() } else { 0 }
    }

    /// Whether `cpu` still owes the open round a flush.
    pub const fn pending(&self, cpu: CpuId) -> bool {
        self.epoch.is_some() && self.in_round(cpu) && !self.has_flushed(cpu)
    }

    /// The last epoch every core it was asked of has flushed, which is the
    /// number [`retire`](crate::va::Space::retire) takes.
    pub const fn retired(&self) -> Epoch {
        self.retired
    }

    /// How many rounds have completed.
    pub const fn rounds(&self) -> u64 {
        self.rounds
    }

    const fn in_round(&self, cpu: CpuId) -> bool {
        cpu.index() < Self::LIMIT as usize && self.asked & (1 << cpu.index()) != 0
    }

    const fn has_flushed(&self, cpu: CpuId) -> bool {
        cpu.index() < Self::LIMIT as usize && self.flushed & (1 << cpu.index()) != 0
    }
}

/// Dropping what a core cached about the address space.
///
/// Static, because the core that flushes is the core that calls it, and the
/// task another core runs on molt's behalf holds no handle to the platform.
///
/// # Safety
///
/// The implementation must return with the calling core holding no translation
/// it cached before the call — global entries included, since the addresses
/// this protocol frees are the kernel's own and a flush that spares them did
/// nothing.
pub unsafe trait Tlb {
    /// Drops every translation this core cached.
    fn flush();
}

#[cfg(test)]
mod tests {
    use super::{Error, Shootdown};
    use crate::CpuId;
    use crate::va::Epoch;

    /// The cores QEMU is given in the smoke, named the way the kernel does.
    const CORES: [CpuId; 4] = [CpuId::new(0), CpuId::new(1), CpuId::new(2), CpuId::new(3)];

    fn epoch(count: u64) -> Epoch {
        (0..count).fold(Epoch::FIRST, |epoch, _| epoch.next())
    }

    #[test]
    fn address_free_only_after_last_flush() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();
        let first = epoch(1);

        let asked = shootdown.begin(first, CORES.into_iter())?;

        assert_eq!(asked, 4);
        assert_eq!(shootdown.epoch(), Some(first));
        for cpu in &CORES[..3] {
            assert_eq!(shootdown.acknowledge(*cpu)?, None, "a round closed with cores still owing");
        }
        assert_eq!(shootdown.outstanding(), 1);
        assert_eq!(shootdown.retired(), Epoch::FIRST, "an epoch retired before the last flush");

        assert_eq!(shootdown.acknowledge(CORES[3])?, Some(first));
        assert_eq!(shootdown.retired(), first);
        assert_eq!(shootdown.outstanding(), 0);
        assert_eq!(shootdown.rounds(), 1);
        Ok(())
    }

    #[test]
    fn one_core_answering_twice_is_not_two_cores() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();
        shootdown.begin(epoch(1), CORES.into_iter())?;

        for _ in 0..8 {
            assert_eq!(
                shootdown.acknowledge(CpuId::BOOT)?,
                None,
                "one core closed a four-core round"
            );
        }

        assert_eq!(shootdown.outstanding(), 3);
        assert!(!shootdown.pending(CpuId::BOOT), "the core that flushed still owes one");
        assert!(shootdown.pending(CORES[1]));
        Ok(())
    }

    #[test]
    fn unmapping_core_owes_flush() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();

        shootdown.begin(epoch(1), CORES.into_iter())?;

        assert!(shootdown.pending(CpuId::BOOT), "the core that freed the range was trusted");
        Ok(())
    }

    #[test]
    fn unasked_core_cannot_close_round() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();
        shootdown.begin(epoch(1), CORES[..2].iter().copied())?;

        assert_eq!(shootdown.acknowledge(CORES[3]), Err(Error::Foreign));
        assert_eq!(shootdown.acknowledge(CpuId::new(64)), Err(Error::Foreign));

        assert_eq!(shootdown.outstanding(), 2, "a foreign answer counted as a flush");
        Ok(())
    }

    #[test]
    fn second_round_waits_for_first() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();
        let first = epoch(1);
        shootdown.begin(first, CORES.into_iter())?;

        assert_eq!(shootdown.begin(epoch(2), CORES.into_iter()), Err(Error::Open));

        for cpu in CORES {
            shootdown.acknowledge(cpu)?;
        }
        assert_eq!(shootdown.begin(epoch(2), CORES.into_iter()), Ok(4));
        assert_eq!(shootdown.retired(), first, "the open round retired its epoch early");
        Ok(())
    }

    #[test]
    fn retired_epoch_not_flushed_again() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();
        let first = epoch(1);
        shootdown.begin(first, CORES.into_iter())?;
        for cpu in CORES {
            shootdown.acknowledge(cpu)?;
        }

        assert_eq!(shootdown.begin(first, CORES.into_iter()), Err(Error::Stale));
        assert_eq!(shootdown.epoch(), None);
        Ok(())
    }

    #[test]
    fn unasked_flush_refused() {
        let mut shootdown = Shootdown::new();

        assert_eq!(shootdown.acknowledge(CpuId::BOOT), Err(Error::Closed));
        assert_eq!(shootdown.outstanding(), 0);
        assert_eq!(shootdown.epoch(), None);
    }

    #[test]
    fn round_over_no_cores_refused() {
        let mut shootdown = Shootdown::new();

        assert_eq!(shootdown.begin(epoch(1), [].into_iter()), Err(Error::Empty));
        assert_eq!(
            shootdown.begin(epoch(1), [CpuId::new(Shootdown::LIMIT as u16)].into_iter()),
            Err(Error::Width),
            "a core the mask cannot name was counted as flushed"
        );
        assert_eq!(shootdown.epoch(), None, "a refused round stayed open");
    }
}
