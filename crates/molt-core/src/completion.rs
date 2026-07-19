//! Bounded, lock-free completion and waker ownership managed by the executor side.
//!
//! Each slot is an atomic state machine rather than a spin lock, so the slab is
//! safe to touch from interrupt context and cannot deadlock under timer
//! preemption or SMP. A slot moves through `EMPTY -> RESERVING -> OCCUPIED`,
//! optionally to `OCCUPIED | READY` once a result is published, and back to
//! `EMPTY` when the result is consumed or the request is cancelled. A transient
//! `CLAIM` flag gives whichever party performs the terminal transition
//! exclusive, wait-free access to the slot's value; the waker is handled
//! separately by the lock-free [`AtomicWaker`]. Acquire/Release pairing on the
//! state word publishes the [`UnsafeCell`] payload between producer and consumer.

use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use core::task::{Context, Poll};

use crate::ring::RequestId;
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

/// The slot holds a reserved request; its value cell is uninitialized of result.
const OCCUPIED: u8 = 0b0001;
/// A result has been published into the value cell (implies `OCCUPIED`).
const READY: u8 = 0b0010;
/// A terminal transition (complete, consume, or cancel) owns the value cell.
const CLAIM: u8 = 0b0100;
/// A reservation is mid-flight; the request id is not yet published.
const RESERVING: u8 = 0b1000;

struct Slot<C> {
    state: AtomicU8,
    request_id: AtomicU64,
    result: UnsafeCell<Option<C>>,
    waker: AtomicWaker,
}

// SAFETY: `state` serializes every access to `result`. The `RESERVING` and
// `CLAIM` flags grant a single party exclusive access across each critical
// transition, and Acquire/Release pairing on `state` publishes writes to the
// `UnsafeCell` before another party observes the corresponding state.
unsafe impl<C: Send> Sync for Slot<C> {}

impl<C> Slot<C> {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            request_id: AtomicU64::new(0),
            result: UnsafeCell::new(None),
            waker: AtomicWaker::new(),
        }
    }
}

/// Fixed-capacity executor-owned waker and completion slab.
pub struct CompletionSlab<C, const N: usize> {
    next_request: AtomicU64,
    slots: [Slot<C>; N],
}

impl<C, const N: usize> CompletionSlab<C, N> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "a completion slab needs at least one slot") };
        Self { next_request: AtomicU64::new(1), slots: [const { Slot::new() }; N] }
    }

    pub fn reserve(&self) -> Result<CompletionToken, CompletionError> {
        for (index, slot) in self.slots.iter().enumerate() {
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

    pub fn wait(&self, token: CompletionToken) -> CompletionFuture<'_, C, N> {
        CompletionFuture { slab: self, token, done: false }
    }

    pub fn complete(&self, request_id: RequestId, result: C) -> Result<(), CompletionError> {
        let target = request_id.get();
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) != OCCUPIED {
                continue;
            }
            if slot.request_id.load(Ordering::Relaxed) != target {
                continue;
            }
            // Claim exclusive access before writing the result; on success no
            // other party can consume, cancel, or reserve this slot.
            if slot
                .state
                .compare_exchange(OCCUPIED, OCCUPIED | CLAIM, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // Re-check the id: the slot could have been cancelled and reused
            // (EMPTY -> OCCUPIED) between the load above and the claim.
            if slot.request_id.load(Ordering::Relaxed) != target {
                slot.state.store(OCCUPIED, Ordering::Release);
                continue;
            }
            // SAFETY: `CLAIM` grants exclusive access to the value cell.
            unsafe {
                *slot.result.get() = Some(result);
            }
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
        if Self::claim_and_clear(slot, token.request_id.get()) {
            Ok(())
        } else {
            Err(CompletionError::Stale)
        }
    }

    pub fn cancel_all(&self) -> usize {
        let mut cancelled = 0;
        for slot in &self.slots {
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
                // A terminal transition is briefly in flight; it neither blocks
                // nor allocates, so retry until it releases the claim.
                core::hint::spin_loop();
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
            unsafe {
                *slot.result.get() = None;
            }
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
        let target = token.request_id.get();
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if slot.request_id.load(Ordering::Relaxed) != target {
                return Poll::Ready(Err(CompletionError::Cancelled));
            }
            if state & READY != 0 {
                if state & CLAIM != 0 {
                    // A concurrent cancel is clearing the slot; let it win and
                    // report cancellation on the next observation.
                    core::hint::spin_loop();
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
                let result = unsafe { (*slot.result.get()).take() };
                slot.request_id.store(0, Ordering::Relaxed);
                slot.state.store(0, Ordering::Release);
                return match result {
                    Some(result) => Poll::Ready(Ok(result)),
                    None => Poll::Ready(Err(CompletionError::Cancelled)),
                };
            }
            // No result yet: register and re-check so a completion that lands
            // between the check and the registration is not missed.
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

impl<C, const N: usize> Default for CompletionSlab<C, N> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CompletionFuture<'s, C, const N: usize> {
    slab: &'s CompletionSlab<C, N>,
    token: CompletionToken,
    done: bool,
}

impl<C, const N: usize> Future for CompletionFuture<'_, C, N> {
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

impl<C, const N: usize> Drop for CompletionFuture<'_, C, N> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.slab.cancel(self.token);
        }
    }
}
