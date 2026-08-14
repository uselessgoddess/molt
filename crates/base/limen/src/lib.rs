#![cfg_attr(not(any(loom, feature = "threads")), no_std)]

//! The threshold between what runs and what is checked.
//!
//! Under `--cfg loom` these are loom's instrumented primitives; everywhere else
//! they are core's. Lock-free code written against them is model-checked
//! without being written twice — closure-scoped cell access is what lets loom
//! see a race, and [`spin_loop`] yields to its scheduler instead of spinning a
//! core it is pretending to be.
//!
//! `model` is the same idea for the test itself: loom's explorer under
//! `--cfg loom`, and one plain run of the closure otherwise. So a concurrency
//! test is written once and gets both — an ordinary racy run from `cargo test`,
//! and every interleaving from `just loom`.
//!
//! Loom does not model every hardware execution, so a green model check is
//! evidence rather than proof.

pub mod atomic {
    pub use core::sync::atomic::Ordering;
    #[cfg(not(loom))]
    pub use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize};

    #[cfg(loom)]
    pub use loom::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize};
}

/// Waits for a short critical section, yielding to loom's scheduler in tests.
#[inline(always)]
pub fn spin_loop() {
    #[cfg(not(loom))]
    core::hint::spin_loop();
    #[cfg(loom)]
    loom::hint::spin_loop();
}

#[cfg(not(loom))]
mod cell {
    /// Closure-scoped [`core::cell::UnsafeCell`] access that loom can instrument.
    #[derive(Debug, Default)]
    pub struct UnsafeCell<T: ?Sized>(core::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub const fn new(data: T) -> Self {
            Self(core::cell::UnsafeCell::new(data))
        }
    }

    impl<T: ?Sized> UnsafeCell<T> {
        #[inline(always)]
        pub fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*const T) -> R,
        {
            f(self.0.get())
        }

        #[inline(always)]
        pub fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(*mut T) -> R,
        {
            f(self.0.get())
        }
    }
}

#[cfg(loom)]
mod cell {
    pub use loom::cell::UnsafeCell;
}

pub use cell::UnsafeCell;

#[cfg(any(loom, feature = "threads"))]
mod door {
    #[cfg(not(loom))]
    pub use std::sync::Arc;
    #[cfg(not(loom))]
    pub use std::thread;

    #[cfg(loom)]
    pub use loom::sync::Arc;
    #[cfg(loom)]
    pub use loom::thread;

    /// Runs `check` under every interleaving loom will explore, or once.
    ///
    /// The bound is loom's, kept here so the two builds accept the same test.
    #[cfg(loom)]
    pub fn model<F: Fn() + Sync + Send + 'static>(check: F) {
        loom::model(check);
    }

    #[cfg(not(loom))]
    pub fn model<F: Fn() + Sync + Send + 'static>(check: F) {
        check();
    }
}

#[cfg(any(loom, feature = "threads"))]
pub use door::{Arc, model, thread};
