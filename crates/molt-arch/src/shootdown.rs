//! Making an address safe to hand out again after it has been taken away.
//!
//! Clearing a leaf does not stop a core from reaching what it translated to.
//! Every core that ever used the address may still hold the translation in its
//! TLB, and no store to a page table invalidates another core's copy — the
//! hardware keeps translations cached per core and does not make them coherent,
//! so the kernel has to. Until every core has been told to drop what it cached,
//! the address still names the frames it used to name on whichever core never
//! heard, and handing the range to a second domain hands that domain the first
//! one's memory.
//!
//! So a revoke is three steps in an order that does not bend: clear the leaf,
//! flush the translation on every core, and only then let the allocator hand the
//! addresses out again. The middle step is [`Tlb::flush`], delivered to the
//! other cores the way any other work reaches them. This module is the
//! bookkeeping that gates the third step on the second: which cores still owe a
//! flush for which [`Epoch`], and what may be
//! [`retire`](crate::va::Space::retire)d once the last one answers.
//!
//! The tracker deliberately holds one open round at a time. Two rounds in flight
//! cannot be told apart by a core that answers with nothing but its own
//! identity, and a round closed by the wrong acknowledgement is exactly the
//! use-after-free the protocol exists to prevent. Batching happens on the other
//! side of it — [`sweep`](crate::va::Space::sweep) closes a batch of releases,
//! however many, into the one epoch a round covers.

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

/// The cores a freed range is still waiting on.
///
/// One tracker covers one machine: the mask is over core indices, so a round is
/// a set of cores and not a count of them, and a core that answers twice cannot
/// close a round the others are still in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shootdown {
    asked: u64,
    flushed: u64,
    epoch: Epoch,
    open: bool,
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

    /// A tracker with nothing outstanding, which is where a boot starts: no
    /// address has been freed, so no core owes anything.
    pub const fn new() -> Self {
        Self {
            asked: 0,
            flushed: 0,
            epoch: Epoch::FIRST,
            open: false,
            retired: Epoch::FIRST,
            rounds: 0,
        }
    }

    /// Opens a round: `epoch` stays unretired until every core in `cores` has
    /// flushed.
    ///
    /// The core doing the unmapping belongs in `cores` like every other. It is
    /// the one likeliest to hold the translation, having just used it, and a
    /// protocol that trusts the initiator to be clean is one that reuses an
    /// address the initiator can still read.
    ///
    /// Returns how many cores the round waits on.
    pub fn begin(
        &mut self,
        epoch: Epoch,
        cores: impl Iterator<Item = CpuId>,
    ) -> Result<u32, Error> {
        if self.open {
            return Err(Error::Open);
        }
        if epoch <= self.retired {
            return Err(Error::Stale);
        }

        // Built to the side and installed whole: a round half-filled with cores
        // is a round that completes before the rest of them have heard.
        let mut asked = 0u64;
        for cpu in cores {
            if cpu.index() >= Self::LIMIT as usize {
                return Err(Error::Width);
            }
            asked |= 1 << cpu.index();
        }
        if asked == 0 {
            return Err(Error::Empty);
        }

        self.asked = asked;
        self.flushed = 0;
        self.epoch = epoch;
        self.open = true;
        Ok(asked.count_ones())
    }

    /// Records that `cpu` has flushed.
    ///
    /// Returns the epoch that became safe to retire, which is `Some` exactly
    /// once per round: on the acknowledgement that leaves nobody owing.
    pub fn acknowledge(&mut self, cpu: CpuId) -> Result<Option<Epoch>, Error> {
        if !self.open {
            return Err(Error::Closed);
        }
        if cpu.index() >= Self::LIMIT as usize || !self.in_round(cpu) {
            return Err(Error::Foreign);
        }

        // A core answering twice is not a second core, which is why this is a
        // set and not a counter: an answer that arrives late, or twice, cannot
        // close a round another core is still in.
        self.flushed |= 1 << cpu.index();
        if self.flushed != self.asked {
            return Ok(None);
        }

        self.open = false;
        self.retired = self.epoch;
        self.rounds += 1;
        Ok(Some(self.epoch))
    }

    /// The epoch the open round covers, if a round is open.
    pub const fn epoch(&self) -> Option<Epoch> {
        match self.open {
            true => Some(self.epoch),
            false => None,
        }
    }

    /// How many cores still owe the open round a flush.
    pub const fn outstanding(&self) -> u32 {
        match self.open {
            true => (self.asked & !self.flushed).count_ones(),
            false => 0,
        }
    }

    /// Whether `cpu` still owes the open round a flush.
    pub const fn pending(&self, cpu: CpuId) -> bool {
        self.open && self.in_round(cpu) && !self.has_flushed(cpu)
    }

    /// The last epoch every core it was asked of has flushed.
    ///
    /// Nothing above this is safe to hand out, which is the number
    /// [`retire`](crate::va::Space::retire) takes.
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
/// The method is static because the core that has to flush is the core that
/// calls it, and it may be reached with nothing but a function pointer — the
/// task another core runs on molt's behalf holds no handle to the platform.
///
/// # Safety
///
/// An implementation must leave the calling core holding no translation it
/// cached before the call, before it returns. Every translation, including the
/// global ones a per-address invalidation is allowed to keep: the addresses this
/// protocol frees are the kernel's own, and a flush that spares them is a flush
/// that did nothing. An implementation that returns early is a use-after-free
/// the hardware then performs on behalf of whoever reads the address next.
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
    fn an_address_is_not_free_until_the_last_core_has_flushed() -> Result<(), Error> {
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
    fn the_core_that_unmapped_owes_a_flush_like_any_other() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();

        shootdown.begin(epoch(1), CORES.into_iter())?;

        assert!(shootdown.pending(CpuId::BOOT), "the core that freed the range was trusted");
        Ok(())
    }

    #[test]
    fn a_core_the_round_never_asked_cannot_close_it() -> Result<(), Error> {
        let mut shootdown = Shootdown::new();
        shootdown.begin(epoch(1), CORES[..2].iter().copied())?;

        assert_eq!(shootdown.acknowledge(CORES[3]), Err(Error::Foreign));
        assert_eq!(shootdown.acknowledge(CpuId::new(64)), Err(Error::Foreign));

        assert_eq!(shootdown.outstanding(), 2, "a foreign answer counted as a flush");
        Ok(())
    }

    #[test]
    fn a_second_round_waits_for_the_first_to_finish() -> Result<(), Error> {
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
    fn an_epoch_already_retired_is_not_flushed_again() -> Result<(), Error> {
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
    fn a_flush_nobody_asked_for_is_refused() {
        let mut shootdown = Shootdown::new();

        assert_eq!(shootdown.acknowledge(CpuId::BOOT), Err(Error::Closed));
        assert_eq!(shootdown.outstanding(), 0);
        assert_eq!(shootdown.epoch(), None);
    }

    #[test]
    fn a_round_over_no_cores_is_refused() {
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
