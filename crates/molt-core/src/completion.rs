//! Bounded, lock-free completion storage.
//!
//! Each slot moves from reservation to occupancy and optionally readiness; a
//! transient `CLAIM` bit serializes completion, consumption, and cancellation.
//! Acquire/Release operations publish the payload without a spin lock.

use core::borrow::Borrow;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::cache::{CacheLayout, Compact, Padded};
use crate::ring::RequestId;
use crate::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use crate::sync::{UnsafeCell, spin_loop};
use crate::waker::AtomicWaker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionError {
    Full,
    Stale,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionToken {
    request_id: RequestId,
    slot: usize,
}

impl CompletionToken {
    pub const fn request_id(self) -> RequestId {
        self.request_id
    }
}

const OCCUPIED: u8 = 0b0001;
const READY: u8 = 0b0010;
const CLAIM: u8 = 0b0100;
const RESERVING: u8 = 0b1000;

struct Slot<C> {
    state: AtomicU8,
    request_id: AtomicU64,
    result: UnsafeCell<Option<C>>,
    waker: AtomicWaker,
}

// SAFETY: `RESERVING` and `CLAIM` serialize `result`, with `state` publishing it.
unsafe impl<C: Send> Sync for Slot<C> {}

impl<C> Slot<C> {
    #[cfg(not(loom))]
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            request_id: AtomicU64::new(0),
            result: UnsafeCell::new(None),
            waker: AtomicWaker::new(),
        }
    }

    #[cfg(loom)]
    fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            request_id: AtomicU64::new(0),
            result: UnsafeCell::new(None),
            waker: AtomicWaker::new(),
        }
    }
}

/// Fixed-capacity executor-owned waker and completion slab.
pub struct CompletionSlab<C, const N: usize, L: CacheLayout = Compact> {
    next_request: AtomicU64,
    slots: [L::Slot<Slot<C>>; N],
}

#[cfg(not(loom))]
impl<C, const N: usize> CompletionSlab<C, N, Compact> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "a completion slab needs at least one slot") };
        Self { next_request: AtomicU64::new(1), slots: [const { Slot::new() }; N] }
    }
}

#[cfg(loom)]
impl<C, const N: usize> CompletionSlab<C, N, Compact> {
    pub fn new() -> Self {
        assert!(N > 0, "a completion slab needs at least one slot");
        Self { next_request: AtomicU64::new(1), slots: core::array::from_fn(|_| Slot::new()) }
    }
}

#[cfg(not(loom))]
impl<C, const N: usize> CompletionSlab<C, N, Padded> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "a completion slab needs at least one slot") };
        Self {
            next_request: AtomicU64::new(1),
            slots: [const { crate::cache::CachePadded::new(Slot::new()) }; N],
        }
    }
}

#[cfg(loom)]
impl<C, const N: usize> CompletionSlab<C, N, Padded> {
    pub fn new() -> Self {
        assert!(N > 0, "a completion slab needs at least one slot");
        Self {
            next_request: AtomicU64::new(1),
            slots: core::array::from_fn(|_| crate::cache::CachePadded::new(Slot::new())),
        }
    }
}

impl<C, const N: usize, L: CacheLayout> CompletionSlab<C, N, L> {
    pub fn reserve(&self) -> Result<CompletionToken, CompletionError> {
        for (index, slot) in self.slots.iter().enumerate() {
            let slot = slot.borrow();
            if slot
                .state
                .compare_exchange(0, RESERVING, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let id = RequestId::new(self.next_request.fetch_add(1, Ordering::Relaxed));
            slot.request_id.store(id.get(), Ordering::Relaxed);
            slot.state.store(OCCUPIED, Ordering::Release);
            return Ok(CompletionToken { request_id: id, slot: index });
        }
        Err(CompletionError::Full)
    }

    pub fn wait(&self, token: CompletionToken) -> CompletionFuture<'_, C, N, L> {
        CompletionFuture { slab: self, token, done: false }
    }

    pub fn complete(&self, request_id: RequestId, result: C) -> Result<(), CompletionError> {
        let target = request_id.get();
        for slot in &self.slots {
            let slot = slot.borrow();
            if slot.state.load(Ordering::Acquire) != OCCUPIED {
                continue;
            }
            if slot.request_id.load(Ordering::Relaxed) != target {
                continue;
            }
            // Claim the slot against concurrent consumption or cancellation.
            if slot
                .state
                .compare_exchange(OCCUPIED, OCCUPIED | CLAIM, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // Cancellation may have reused the slot before the claim.
            if slot.request_id.load(Ordering::Relaxed) != target {
                slot.state.store(OCCUPIED, Ordering::Release);
                continue;
            }
            // SAFETY: `CLAIM` grants exclusive access to the value cell.
            slot.result.with_mut(|value| unsafe {
                *value = Some(result);
            });
            // Release publishes the result before a consumer observes `READY`.
            slot.state.store(OCCUPIED | READY, Ordering::Release);
            slot.waker.wake();
            return Ok(());
        }
        Err(CompletionError::Stale)
    }

    pub fn cancel(&self, token: CompletionToken) -> Result<(), CompletionError> {
        let Some(slot) = self.slots.get(token.slot) else {
            return Err(CompletionError::Stale);
        };
        if Self::claim_and_clear(slot.borrow(), token.request_id.get()) {
            Ok(())
        } else {
            Err(CompletionError::Stale)
        }
    }

    pub fn cancel_all(&self) -> usize {
        let mut cancelled = 0;
        for slot in &self.slots {
            let slot = slot.borrow();
            let id = slot.request_id.load(Ordering::Relaxed);
            if id != 0 && Self::claim_and_clear(slot, id) {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Claims the slot and returns it to `EMPTY`, dropping any pending result
    /// and waking a parked task. Returns `false` if the slot no longer holds
    /// `request_id`.
    fn claim_and_clear(slot: &Slot<C>, request_id: u64) -> bool {
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if state & OCCUPIED == 0 || slot.request_id.load(Ordering::Relaxed) != request_id {
                return false;
            }
            if state & CLAIM != 0 {
                // A terminal transition briefly owns the slot.
                spin_loop();
                continue;
            }
            if slot
                .state
                .compare_exchange_weak(state, state | CLAIM, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            if slot.request_id.load(Ordering::Relaxed) != request_id {
                slot.state.store(state, Ordering::Release);
                return false;
            }
            // SAFETY: the `CLAIM` flag grants exclusive access to the value cell.
            slot.result.with_mut(|value| unsafe {
                *value = None;
            });
            let waker = slot.waker.take();
            slot.request_id.store(0, Ordering::Relaxed);
            slot.state.store(0, Ordering::Release);
            if let Some(waker) = waker {
                waker.wake();
            }
            return true;
        }
    }

    fn poll(
        &self,
        token: CompletionToken,
        cx: &mut Context<'_>,
    ) -> Poll<Result<C, CompletionError>> {
        let Some(slot) = self.slots.get(token.slot) else {
            return Poll::Ready(Err(CompletionError::Cancelled));
        };
        let slot = slot.borrow();
        let target = token.request_id.get();
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if slot.request_id.load(Ordering::Relaxed) != target {
                return Poll::Ready(Err(CompletionError::Cancelled));
            }
            if state & READY != 0 {
                if state & CLAIM != 0 {
                    // Let the concurrent cancellation finish.
                    spin_loop();
                    continue;
                }
                if slot
                    .state
                    .compare_exchange_weak(
                        OCCUPIED | READY,
                        OCCUPIED | READY | CLAIM,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    continue;
                }
                // SAFETY: the `CLAIM` flag grants exclusive access to the value cell.
                let result = slot.result.with_mut(|value| unsafe { (*value).take() });
                slot.request_id.store(0, Ordering::Relaxed);
                slot.state.store(0, Ordering::Release);
                return match result {
                    Some(result) => Poll::Ready(Ok(result)),
                    None => Poll::Ready(Err(CompletionError::Cancelled)),
                };
            }
            // Re-check after registration to close the lost-wakeup window.
            slot.waker.register(cx.waker());
            let recheck = slot.state.load(Ordering::Acquire);
            if slot.request_id.load(Ordering::Relaxed) != target {
                return Poll::Ready(Err(CompletionError::Cancelled));
            }
            if recheck & READY == 0 {
                return Poll::Pending;
            }
        }
    }
}

impl<C, const N: usize> Default for CompletionSlab<C, N, Compact> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C, const N: usize> Default for CompletionSlab<C, N, Padded> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use core::task::{Context, Poll, Waker};

    use loom::sync::Arc;
    use loom::thread;

    use super::CompletionSlab;
    use crate::waker::Flag;

    #[test]
    fn race_wakes_waiter() {
        loom::model(|| {
            let slab = Arc::new(CompletionSlab::<u32, 1>::new());
            let token = slab.reserve().expect("free slot");
            let flag = Flag::new();
            let waker = Waker::from(flag.clone());

            let producer = {
                let slab = slab.clone();
                thread::spawn(move || slab.complete(token.request_id(), 7).expect("live id"))
            };
            let polled = slab.poll(token, &mut Context::from_waker(&waker));
            producer.join().unwrap();

            match polled {
                Poll::Ready(result) => assert_eq!(result, Ok(7)),
                Poll::Pending => assert!(flag.fired(), "parked without a wake"),
            }
        });
    }

    #[test]
    fn race_releases_slot() {
        loom::model(|| {
            let slab = Arc::new(CompletionSlab::<u32, 1>::new());
            let token = slab.reserve().expect("free slot");

            let producer = {
                let slab = slab.clone();
                thread::spawn(move || slab.complete(token.request_id(), 7))
            };
            let cancelled = slab.cancel(token);
            let completed = producer.join().unwrap();

            if cancelled.is_ok() {
                assert!(slab.reserve().is_ok(), "a cancelled slot must be reusable");
            } else {
                assert!(completed.is_ok(), "neither party claimed the slot");
            }
        });
    }
}

pub struct CompletionFuture<'s, C, const N: usize, L: CacheLayout = Compact> {
    slab: &'s CompletionSlab<C, N, L>,
    token: CompletionToken,
    done: bool,
}

impl<C, const N: usize, L: CacheLayout> Future for CompletionFuture<'_, C, N, L> {
    type Output = Result<C, CompletionError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let outcome = this.slab.poll(this.token, cx);
        if outcome.is_ready() {
            this.done = true;
        }
        outcome
    }
}

impl<C, const N: usize, L: CacheLayout> Drop for CompletionFuture<'_, C, N, L> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.slab.cancel(self.token);
        }
    }
}
