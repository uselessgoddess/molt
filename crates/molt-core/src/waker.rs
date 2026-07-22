//! A lock-free, single-consumer waker cell.
//!
//! The `WAITING`/`REGISTERING`/`WAKING` protocol follows
//! `futures::task::AtomicWaker` without adding a runtime dependency.

use core::task::Waker;

use crate::sync::UnsafeCell;
use crate::sync::atomic::{AtomicU8, Ordering};

const WAITING: u8 = 0;
const REGISTERING: u8 = 0b01;
const WAKING: u8 = 0b10;

/// A lock-free cell holding at most one [`Waker`].
///
/// One consumer registers while any number of producers may wake concurrently.
pub struct AtomicWaker {
    state: AtomicU8,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: `REGISTERING` and `WAKING` give one party exclusive access to `waker`.
unsafe impl Send for AtomicWaker {}
// SAFETY: the same state-machine invariant permits shared access.
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(WAITING), waker: UnsafeCell::new(None) }
    }

    #[cfg(loom)]
    pub fn new() -> Self {
        Self { state: AtomicU8::new(WAITING), waker: UnsafeCell::new(None) }
    }

    /// Registers one consumer, preserving a wake that races with registration.
    pub fn register(&self, waker: &Waker) {
        match self.state.compare_exchange(
            WAITING,
            REGISTERING,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.waker.with_mut(|stored| {
                    // SAFETY: `REGISTERING` grants exclusive access until release.
                    let stored = unsafe { &mut *stored };
                    if !stored.as_ref().is_some_and(|current| current.will_wake(waker)) {
                        *stored = Some(waker.clone());
                    }
                });
                match self.state.compare_exchange(
                    REGISTERING,
                    WAITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {}
                    Err(_waking) => {
                        // The producer deferred this wake while registration held the cell.
                        // SAFETY: `REGISTERING | WAKING` excludes every other writer.
                        let waker = self.waker.with_mut(|stored| unsafe { (*stored).take() });
                        self.state.swap(WAITING, Ordering::AcqRel);
                        if let Some(waker) = waker {
                            waker.wake();
                        }
                    }
                }
            }
            // Re-arm after an in-flight wake or an invalid second registration.
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
                // SAFETY: `WAITING -> WAKING` grants exclusive access to the cell.
                let waker = self.waker.with_mut(|stored| unsafe { (*stored).take() });
                self.state.fetch_and(!WAKING, Ordering::Release);
                waker
            }
            // Registration will observe `WAKING`, or another producer owns it.
            _ => None,
        }
    }
}

impl Default for AtomicWaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, loom))]
pub(crate) struct Flag(pub(crate) loom::sync::atomic::AtomicBool);

#[cfg(all(test, loom))]
impl std::task::Wake for Flag {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.store(true, crate::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(all(test, loom))]
impl Flag {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(loom::sync::atomic::AtomicBool::new(false)))
    }

    pub(crate) fn fired(&self) -> bool {
        self.0.load(crate::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use core::task::Waker;

    use loom::sync::Arc;
    use loom::thread;

    use super::{AtomicWaker, Flag};

    #[test]
    fn race_keeps_wake() {
        loom::model(|| {
            let cell = Arc::new(AtomicWaker::new());
            let flag = Flag::new();
            let waker = Waker::from(flag.clone());

            let notifier = {
                let cell = cell.clone();
                thread::spawn(move || cell.wake())
            };
            cell.register(&waker);
            notifier.join().unwrap();

            assert!(
                flag.fired() || cell.take().is_some(),
                "the wake neither fired the waker nor left it registered"
            );
        });
    }
}
