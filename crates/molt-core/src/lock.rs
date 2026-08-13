//! A ticket lock, for the things a machine has exactly one of.
//!
//! The rest of this crate is lock-free because it sits on a hot path. This does
//! not: what it guards are the machine-wide tables — the address space above
//! all — which are cut once and touched rarely, and where a core that waits is
//! cheaper than a core that has to retry a transaction. Tickets rather than a
//! test-and-set flag because the waiting is what has to be bounded: a core takes
//! a number and is served in the order the numbers were taken, so an unlucky
//! core waits for the cores ahead of it and never for the ones behind.
//!
//! The critical sections here are short and hold no interrupt state. A lock a
//! handler may also take needs the interrupt mask saved across it, which this
//! deliberately does not do.

use core::ops::{Deref, DerefMut};

use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{UnsafeCell, spin_loop};

/// Mutual exclusion by ticket, over data one core touches at a time.
pub struct Spinlock<T: ?Sized> {
    next: AtomicU64,
    serving: AtomicU64,
    data: UnsafeCell<T>,
}

// SAFETY: the ticket pair hands the data to one core at a time, so sharing the
// lock is sharing the data by move rather than by reference.
unsafe impl<T: ?Sized + Send> Send for Spinlock<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Sync for Spinlock<T> {}

#[cfg(not(loom))]
impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Self { next: AtomicU64::new(0), serving: AtomicU64::new(0), data: UnsafeCell::new(data) }
    }
}

#[cfg(loom)]
impl<T> Spinlock<T> {
    pub fn new(data: T) -> Self {
        Self { next: AtomicU64::new(0), serving: AtomicU64::new(0), data: UnsafeCell::new(data) }
    }
}

impl<T: ?Sized> Spinlock<T> {
    /// Takes a ticket and waits for it to be called.
    pub fn lock(&self) -> Guard<'_, T> {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            spin_loop();
        }
        Guard { lock: self, ticket }
    }

    /// Takes the lock only if nobody is holding or waiting for it.
    ///
    /// A caller that cannot wait — an interrupt handler reporting, a panic
    /// path dumping state — gets `None` rather than a deadlock.
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        let ticket = self.serving.load(Ordering::Relaxed);
        self.next
            .compare_exchange(ticket, ticket + 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        Some(Guard { lock: self, ticket })
    }
}

/// Exclusive access, until it is dropped.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct Guard<'lock, T: ?Sized> {
    lock: &'lock Spinlock<T>,
    ticket: u64,
}

impl<T: ?Sized> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard is the one being served, so nothing else holds a
        // reference to the data.
        self.lock.data.with(|data| unsafe { &*data })
    }
}

impl<T: ?Sized> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` makes this guard's own borrow
        // unique as well.
        self.lock.data.with_mut(|data| unsafe { &mut *data })
    }
}

impl<T: ?Sized> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // The release publishes everything written under the lock to whoever
        // holds the next ticket, which is the only core that will read it.
        self.lock.serving.store(self.ticket.wrapping_add(1), Ordering::Release);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::Spinlock;

    #[test]
    fn held_lock_turns_away_try() {
        let lock = Spinlock::new(0u64);
        let held = lock.lock();
        assert!(lock.try_lock().is_none(), "a held lock was handed out twice");
        drop(held);
        assert!(lock.try_lock().is_some(), "a released lock stayed shut");
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;

    use super::Spinlock;

    #[test]
    fn exclusion_holds_under_every_interleaving() {
        loom::model(|| {
            let lock = Arc::new(Spinlock::new(0u64));
            let other = {
                let lock = lock.clone();
                thread::spawn(move || *lock.lock() += 1)
            };
            *lock.lock() += 1;
            other.join().expect("a thread that only counted");

            assert_eq!(*lock.lock(), 2, "an increment was lost under the lock");
        });
    }
}
