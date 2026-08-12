//! The machine's address space, of which there is one.
//!
//! A single address space is the premise the rest of the design rests on: an
//! address means the same thing on every core and in every domain, so a grant
//! moves no bytes and a pointer keeps its meaning when it crosses a boundary
//! (`docs/address-space.md`). That only holds while one allocator hands out
//! every address there is. Two of them, each cutting the same range into the
//! same arenas, would hand two subsystems the same address and call both of
//! them unique — so the space lives here, behind a lock, and callers borrow it.
//!
//! The lock is a ticket lock ([`Spinlock`]) because allocation is rare and
//! contention should be fair rather than fast: cutting an extent out is a few
//! hundred cycles against a boot-long life, and a core that keeps losing a
//! test-and-set race would be starved of addresses by cores that already have
//! them.

use core::cell::UnsafeCell;

use molt_arch::va::{Hole, Space, Widths};
use molt_core::lock::{Guard, Spinlock};

/// Free ranges per class, which is the budget `docs/va-allocator.md` sizes:
/// 24 bytes apiece, 64 per class, 4 608 bytes in total.
const HOLES: usize = 3 * 64;

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
