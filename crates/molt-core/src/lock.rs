//! A ticket lock, for the things a machine has exactly one of.
//!
//! The rest of this crate is lock-free because it is on a hot path; this guards
//! the machine-wide tables, cut once and touched rarely, where a core that waits
//! is cheaper than one retrying a transaction. Tickets rather than test-and-set
//! because the *waiting* is what has to be bounded: served in the order numbers
//! were taken, an unlucky core waits only for the cores ahead of it.
//!
//! Critical sections here are short and hold no interrupt state. A lock a
//! handler may also take needs the interrupt mask saved across it, which this
//! deliberately does not do.

use core::ops::{Deref, DerefMut};

use limen::UnsafeCell;
use limen::atomic::{AtomicU64, Ordering};

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
            limen::spin_loop();
        }
        Guard { lock: self, ticket }
    }

    /// Takes the lock only if nobody is holding or waiting for it, so a caller
    /// that cannot wait — an interrupt handler, a panic path — gets `None`
    /// rather than a deadlock.
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

#[cfg(test)]
mod races {
    use limen::{Arc, thread};

    use super::Spinlock;

    #[test]
    fn exclusion_holds_under_every_interleaving() {
        limen::model(|| {
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
