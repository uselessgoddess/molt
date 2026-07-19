//! A lock-free single-slot waker cell.
//!
//! [`AtomicWaker`] coordinates exactly one registering task with any number of
//! notifying producers without a lock. It is the waker half of the lock-free
//! [`crate::completion`] slab: the task side calls [`AtomicWaker::register`]
//! while a driver or interrupt calls [`AtomicWaker::wake`], and no notification
//! is lost even when the two race.
//!
//! The three-state registration protocol (`WAITING`, `REGISTERING`, `WAKING`)
//! follows the well-known design used by `futures::task::AtomicWaker`; it is
//! reproduced here so the kernel keeps a single audited `no_std` primitive with
//! no external dependency.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::Waker;

/// No task is registering and no wake is in flight.
const WAITING: u8 = 0;
/// The single consumer holds the cell to store its [`Waker`].
const REGISTERING: u8 = 0b01;
/// A producer holds the cell to take and fire the stored [`Waker`].
const WAKING: u8 = 0b10;

/// A lock-free cell holding at most one [`Waker`].
///
/// One consumer registers its waker; any producer may wake it. Registration and
/// wakeup may run concurrently: if a wake arrives while a registration is in
/// progress, the registering side observes the `WAKING` flag and fires the waker
/// itself, so the notification is never dropped.
pub struct AtomicWaker {
    state: AtomicU8,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: the `state` machine gives at most one party mutable access to `waker`
// at a time. `REGISTERING` is exclusive to the single consumer and `WAKING` is
// claimed with a read-modify-write, so the `UnsafeCell` is never aliased.
unsafe impl Send for AtomicWaker {}
// SAFETY: see the `Send` justification above.
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(WAITING), waker: UnsafeCell::new(None) }
    }

    /// Registers `waker` to be notified by the next [`wake`](Self::wake).
    ///
    /// Only one consumer may call this at a time. If a concurrent `wake`
    /// happens during registration, `waker` is fired before returning so the
    /// caller is always eventually polled again.
    pub fn register(&self, waker: &Waker) {
        match self.state.compare_exchange(
            WAITING,
            REGISTERING,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: the `REGISTERING` claim grants this consumer exclusive
                // access to the waker cell until the release compare-exchange.
                unsafe {
                    let stored = &mut *self.waker.get();
                    if !stored.as_ref().is_some_and(|current| current.will_wake(waker)) {
                        *stored = Some(waker.clone());
                    }
                }
                match self.state.compare_exchange(
                    REGISTERING,
                    WAITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {}
                    Err(_waking) => {
                        // A `wake` set the `WAKING` bit while we registered; it
                        // could not touch the cell, so we deliver the wakeup.
                        // SAFETY: the observed `REGISTERING | WAKING` state still
                        // excludes every other writer from the waker cell.
                        let waker = unsafe { (*self.waker.get()).take() };
                        self.state.swap(WAITING, Ordering::AcqRel);
                        if let Some(waker) = waker {
                            waker.wake();
                        }
                    }
                }
            }
            // A wake is in flight, or another registration is racing (which the
            // single-consumer contract forbids). Either way, re-arm ourselves so
            // no wakeup is lost.
            Err(_) => waker.wake_by_ref(),
        }
    }

    /// Wakes the registered task, if any. Safe to call from interrupt context.
    pub fn wake(&self) {
        if let Some(waker) = self.take() {
            waker.wake();
        }
    }

    /// Removes the registered waker without firing it.
    pub fn take(&self) -> Option<Waker> {
        match self.state.fetch_or(WAKING, Ordering::AcqRel) {
            WAITING => {
                // SAFETY: transitioning `WAITING -> WAKING` claims the cell; no
                // consumer can be registering and no other waker can be taking.
                let waker = unsafe { (*self.waker.get()).take() };
                self.state.fetch_and(!WAKING, Ordering::Release);
                waker
            }
            // The consumer is registering (it will notice `WAKING`) or another
            // producer already claimed the wake; nothing to do here.
            _ => None,
        }
    }
}

impl Default for AtomicWaker {
    fn default() -> Self {
        Self::new()
    }
}
