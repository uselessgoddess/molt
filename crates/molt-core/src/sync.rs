//! Synchronization primitives with loom substitutes under `--cfg loom`.
//!
//! Closure-scoped cell access exposes races to loom, and [`spin_loop`] yields
//! to its scheduler. Loom does not model every hardware execution, so a green
//! model check is evidence rather than proof.

pub(crate) mod atomic {
    // `Ordering` needs no instrumentation, so both builds use core's enum.
    pub(crate) use core::sync::atomic::Ordering;
    #[cfg(not(loom))]
    pub(crate) use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize};

    #[cfg(loom)]
    pub(crate) use loom::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize};
}

/// Waits for a short critical section, yielding to loom's scheduler in tests.
#[inline(always)]
pub(crate) fn spin_loop() {
    #[cfg(not(loom))]
    core::hint::spin_loop();
    #[cfg(loom)]
    loom::thread::yield_now();
}

#[cfg(not(loom))]
mod cell {
    /// Closure-scoped [`core::cell::UnsafeCell`] access that loom can instrument.
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
