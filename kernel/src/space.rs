//! The machine's address space, of which there is one.
//!
//! One global address space is the premise everything else rests on
//! (`docs/address-space.md`), and it holds only while one allocator hands out
//! every address there is: two of them, cutting the same range into the same
//! arenas, would hand two subsystems the same address and call both unique. So
//! the space lives here behind a lock and callers borrow it.
//!
//! A ticket lock ([`Spinlock`]), because allocation is rare and the waiting is
//! what has to be bounded: a core that kept losing a test-and-set race would be
//! starved of addresses by the cores that already have them.
//!
//! [`recycle`] is the other thing here, because giving an extent back is three
//! steps in an order that does not bend and no caller should be writing them out
//! again.

use core::cell::UnsafeCell;

use molt_arch::Shootdown;
use molt_arch::va::{Epoch, Extent, Hole, Space, Widths};
use molt_core::lock::{Guard, Spinlock};
use molt_exec::Executor;

use crate::config::HOLES;
use crate::smp;

/// The free lists themselves, which the space borrows for the life of the
/// machine. They sit outside the lock because nothing may name them except
/// [`cut`], which runs once and while holding it.
struct Holes(UnsafeCell<[Hole; HOLES]>);

// SAFETY: `cut` is the only reader, it holds the lock, and it runs once.
unsafe impl Sync for Holes {}

static HOLES_STORAGE: Holes = Holes(UnsafeCell::new([Hole::EMPTY; HOLES]));

static SPACE: Spinlock<Option<Space<'static>>> = Spinlock::new(None);

/// Cuts the one address space out of the width the hardware reported.
///
/// Called once, before any core other than the boot core is running. A second
/// call is a bug rather than a no-op: it would mean some subsystem believed it
/// owned the address space.
pub fn cut(widths: Widths) {
    let mut space = SPACE.lock();
    assert!(space.is_none(), "the machine's address space was cut twice");
    // SAFETY: the lock is held, this is the only path that names the storage,
    // and it hands the borrow to the one space that outlives every caller.
    let holes = unsafe { &mut *HOLES_STORAGE.0.get() };
    *space = Some(Space::over(widths.address(), holes).expect("a space wide enough to cut"));
}

/// Borrows the address space, waiting for whoever holds it.
pub fn global() -> Global {
    Global(SPACE.lock())
}

/// The address space, held.
pub struct Global(Guard<'static, Option<Space<'static>>>);

impl core::ops::Deref for Global {
    type Target = Space<'static>;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("the address space, cut at boot")
    }
}

impl core::ops::DerefMut for Global {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("the address space, cut at boot")
    }
}

/// Gives `extent` back and holds its addresses until every core has flushed.
///
/// The three steps of a revoke, in the one order that is safe, so that no caller
/// writes them out again: release into the open batch, flush every attending
/// core, retire. Returns the epoch retired and the cores it waited on.
///
/// The addresses are asserted, not assumed, to stay out of circulation for the
/// whole of the middle step — reversing the last two is a use-after-free the
/// hardware performs for whoever gets the addresses next.
pub fn recycle(exec: &Executor, space: &mut Space<'_>, extent: Extent) -> (Epoch, u32) {
    let (class, bytes) = (extent.class(), extent.bytes());
    let circulating = space.free(class);
    space.release(extent).expect("an extent this space issued");
    let epoch = space.sweep();

    let mut round = Shootdown::new();
    let cores = round.begin(epoch, smp::attending()).expect("a core to flush");
    assert!(round.pending(smp::cpu()), "the core that did the unmapping was trusted");
    // Not `==`: a release with no slot left to record it in merges into a
    // neighbouring free range and takes it into quarantine too, which is fewer
    // free bytes than before rather than more.
    assert!(space.free(class) <= circulating, "a freed address was reusable before any flush");
    assert!(space.quarantined(class) >= bytes, "the freed range never reached quarantine");

    let retired = smp::close(exec, &mut round);
    assert_eq!(retired, epoch, "a round retired an epoch it was not opened for");
    space.retire(retired);
    assert!(space.free(class) >= circulating + bytes, "a flushed range stayed out of circulation");
    (retired, cores)
}
