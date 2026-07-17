//! Bounded completion and waker ownership managed by the executor side.

use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use crate::ring::RequestId;

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

struct Slot<C> {
    request_id: Option<RequestId>,
    result: Option<C>,
    waker: Option<Waker>,
}

impl<C> Slot<C> {
    const fn new() -> Self {
        Self { request_id: None, result: None, waker: None }
    }

    fn clear(&mut self) {
        self.request_id = None;
        self.result = None;
        self.waker = None;
    }
}

struct LockedSlot<C> {
    locked: AtomicBool,
    value: UnsafeCell<Slot<C>>,
}

// SAFETY: every access to the slot value is serialized by `locked`.
unsafe impl<C: Send> Sync for LockedSlot<C> {}

impl<C> LockedSlot<C> {
    const fn new() -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(Slot::new()) }
    }

    fn with<R>(&self, operation: impl FnOnce(&mut Slot<C>) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: this lock is the only path to `value`, and it is held exclusively.
        let result = operation(unsafe { &mut *self.value.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

/// Fixed-capacity executor-owned waker and completion slab.
pub struct CompletionSlab<C, const N: usize> {
    next_request: AtomicU64,
    slots: [LockedSlot<C>; N],
}

impl<C, const N: usize> CompletionSlab<C, N> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "a completion slab needs at least one slot") };
        Self { next_request: AtomicU64::new(1), slots: [const { LockedSlot::new() }; N] }
    }

    pub fn reserve(&self) -> Result<CompletionToken, CompletionError> {
        for (index, slot) in self.slots.iter().enumerate() {
            let request_id = slot.with(|slot| {
                if slot.request_id.is_some() {
                    return None;
                }
                let id = RequestId::new(self.next_request.fetch_add(1, Ordering::Relaxed));
                slot.request_id = Some(id);
                Some(id)
            });
            if let Some(request_id) = request_id {
                return Ok(CompletionToken { request_id, slot: index });
            }
        }
        Err(CompletionError::Full)
    }

    pub fn wait(&self, token: CompletionToken) -> CompletionFuture<'_, C, N> {
        CompletionFuture { slab: self, token, done: false }
    }

    pub fn complete(&self, request_id: RequestId, result: C) -> Result<(), CompletionError> {
        let mut pending = Some(result);
        for slot in &self.slots {
            let waker = slot.with(|slot| {
                if slot.request_id != Some(request_id) || slot.result.is_some() {
                    return None;
                }
                slot.result = pending.take();
                slot.waker.take()
            });
            if pending.is_none() {
                if let Some(waker) = waker {
                    waker.wake();
                }
                return Ok(());
            }
        }
        Err(CompletionError::Stale)
    }

    pub fn cancel(&self, token: CompletionToken) -> Result<(), CompletionError> {
        let Some(slot) = self.slots.get(token.slot) else {
            return Err(CompletionError::Stale);
        };
        let (matched, waker) = slot.with(|slot| {
            if slot.request_id != Some(token.request_id) {
                return (false, None);
            }
            let waker = slot.waker.take();
            slot.clear();
            (true, waker)
        });
        if !matched {
            return Err(CompletionError::Stale);
        }
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    pub fn cancel_all(&self) -> usize {
        let mut cancelled = 0;
        for slot in &self.slots {
            let waker = slot.with(|slot| {
                slot.request_id?;
                cancelled += 1;
                let waker = slot.waker.take();
                slot.clear();
                Some(waker)
            });
            if let Some(Some(waker)) = waker {
                waker.wake();
            }
        }
        cancelled
    }
}

impl<C, const N: usize> Default for CompletionSlab<C, N> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CompletionFuture<'slab, C, const N: usize> {
    slab: &'slab CompletionSlab<C, N>,
    token: CompletionToken,
    done: bool,
}

impl<C, const N: usize> Future for CompletionFuture<'_, C, N> {
    type Output = Result<C, CompletionError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let Some(slot) = this.slab.slots.get(this.token.slot) else {
            this.done = true;
            return Poll::Ready(Err(CompletionError::Cancelled));
        };
        let outcome = slot.with(|slot| {
            if slot.request_id != Some(this.token.request_id) {
                return Poll::Ready(Err(CompletionError::Cancelled));
            }
            if let Some(result) = slot.result.take() {
                slot.clear();
                return Poll::Ready(Ok(result));
            }
            if !slot.waker.as_ref().is_some_and(|waker| waker.will_wake(cx.waker())) {
                slot.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        });
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
