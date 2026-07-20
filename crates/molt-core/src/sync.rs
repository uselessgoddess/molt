//! Synchronization primitives, swapped for loom's model-checked equivalents
//! under `--cfg loom`.
//!
//! Molt's queues, slabs and wakers are hand-written lock-free code, so their
//! correctness rests on orderings that a normal test run cannot exercise: a
//! passing test only proves that one interleaving on one memory model worked.
//! loom replaces the atomics with instrumented ones and runs a test body once
//! per distinct execution the C11 model permits, so a lost wakeup shows up on
//! a laptop instead of on hardware months later.
//!
//! Everything here exists to keep that swap invisible to the primitives:
//!
//! - [`UnsafeCell`] exposes `with`/`with_mut` closures instead of a raw `get`,
//!   because loom must know when a cell is accessed to detect a data race.
//! - [`spin_loop`] yields under loom; loom's scheduler is deliberately unfair,
//!   so a spin loop that never yields never makes progress and the test hangs.
//! - Constructors use an ordinary `cfg(loom)` branch because loom's atomics
//!   are not const-constructible.
//!
//! Two limits are worth knowing before trusting a green loom run: it models
//! `SeqCst` as `AcqRel`, so a `SeqCst`-dependent bug can slip through, and it
//! does not explore load-buffering executions. loom raises confidence a long
//! way; it is not a proof.

pub(crate) mod atomic {
    // `Ordering` is a plain enum with no instrumentation; loom re-exports
    // core's, so taking it from core directly keeps the two paths identical.
    pub(crate) use core::sync::atomic::Ordering;
    #[cfg(not(loom))]
    pub(crate) use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize};

    #[cfg(loom)]
    pub(crate) use loom::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize};
}

/// Hints that the caller is waiting for another party to finish a short,
/// non-blocking critical section.
///
/// Under loom this is a yield: loom will otherwise run the spinning thread
/// forever and the model never terminates.
#[inline(always)]
pub(crate) fn spin_loop() {
    #[cfg(not(loom))]
    core::hint::spin_loop();
    #[cfg(loom)]
    loom::thread::yield_now();
}

#[cfg(not(loom))]
mod cell {
    /// An [`core::cell::UnsafeCell`] restricted to closure-scoped access.
    ///
    /// The closures carry no cost here; they exist so the loom build can see
    /// where each access begins and ends.
    #[derive(Debug, Default)]
    pub(crate) struct UnsafeCell<T: ?Sized>(core::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub(crate) const fn new(data: T) -> Self {
            Self(core::cell::UnsafeCell::new(data))
        }
    }

    impl<T: ?Sized> UnsafeCell<T> {
        #[inline(always)]
        pub(crate) fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*const T) -> R,
        {
            f(self.0.get())
        }

        #[inline(always)]
        pub(crate) fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*mut T) -> R,
        {
            f(self.0.get())
        }
    }
}

#[cfg(loom)]
mod cell {
    pub(crate) use loom::cell::UnsafeCell;
}

pub(crate) use cell::UnsafeCell;
