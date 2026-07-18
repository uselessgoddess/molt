//! Allocation-free bounded ready scheduling.
//!
//! Each task owns one [`AtomicU8`] slot state. Packing several slots into a
//! single `AtomicU64` was considered to shrink the scan and enable SIMD-style
//! word loads, but rejected: at the `1..=256` capacity here the whole array is
//! at most 256 bytes (four cache lines), so the linear scan is already cheap,
//! while a shared word would make [`wake`](Executor::wake) — a lock-free path
//! reachable from interrupt context — contend across otherwise independent
//! tasks and turn every slot's compare-exchange into a retry against its
//! neighbours. Per-slot atomics keep each task's state machine independent and
//! free of that false sharing, which is worth more than a marginally tighter
//! scan.
//!
//! [`Executor::waker`] bridges this ready queue to [`core::task::Waker`]. The
//! returned waker carries a pointer to a per-task [`Handle`] living inside the
//! executor rather than an owned, cloned [`Waker`]: waking is a direct
//! [`Executor::wake`] bit-set, cloning copies one pointer, and dropping is a
//! no-op. That keeps the completion hot path free of allocation, waker copies,
//! and any user vtable indirection beyond the single unavoidable [`RawWaker`]
//! call, which is the [`TaskId`]-direct integration Stage 1 calls for.

use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{RawWaker, RawWakerVTable, Waker};

const OCCUPIED: u8 = 1 << 0;
const READY: u8 = 1 << 1;
const POLLING: u8 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskId(u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    Full,
}

/// A per-task waker cookie co-located with the ready queue.
///
/// A [`Waker`] built by [`Executor::waker`] points at one of these. It records
/// only the task index; the owning executor is recovered from the handle's own
/// address, so the cookie needs no back-pointer and the executor stays
/// const-constructible.
#[derive(Clone, Copy)]
struct Handle {
    index: u8,
}

/// A bounded task registry and ready queue represented by atomic slot states.
pub struct Executor<const N: usize> {
    states: [AtomicU8; N],
    handles: [Handle; N],
}

impl<const N: usize> Executor<N> {
    pub const fn new() -> Self {
        const { assert!(N > 0 && N <= 256, "executor capacity must be in 1..=256") };
        let mut handles = [Handle { index: 0 }; N];
        let mut index = 0;
        while index < N {
            handles[index].index = index as u8;
            index += 1;
        }
        Self { states: [const { AtomicU8::new(0) }; N], handles }
    }

    pub fn register(&self) -> Result<TaskId, SpawnError> {
        for (index, state) in self.states.iter().enumerate() {
            if state.compare_exchange(0, OCCUPIED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Ok(TaskId(index as u8));
            }
        }
        Err(SpawnError::Full)
    }

    pub fn unregister(&self, task: TaskId) {
        if let Some(state) = self.states.get(task.0 as usize) {
            state.store(0, Ordering::Release);
        }
    }

    pub fn wake(&self, task: TaskId) {
        let Some(state) = self.states.get(task.0 as usize) else {
            return;
        };
        let mut current = state.load(Ordering::Acquire);
        while current & OCCUPIED != 0 && current & READY == 0 {
            match state.compare_exchange_weak(
                current,
                current | READY,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn next_ready(&self) -> Option<TaskId> {
        for (index, state) in self.states.iter().enumerate() {
            let mut current = state.load(Ordering::Acquire);
            while current & (OCCUPIED | READY | POLLING) == (OCCUPIED | READY) {
                let polling = (current & !READY) | POLLING;
                match state.compare_exchange_weak(
                    current,
                    polling,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(TaskId(index as u8)),
                    Err(actual) => current = actual,
                }
            }
        }
        None
    }

    /// Marks the task's poll complete, preserving any wake that arrived during it.
    pub fn complete_poll(&self, task: TaskId) {
        if let Some(state) = self.states.get(task.0 as usize) {
            state.fetch_and(!POLLING, Ordering::Release);
        }
    }

    /// Builds a [`Waker`] that marks `task` ready in this executor.
    ///
    /// Waking it is a direct [`Executor::wake`] atomic bit-set with no
    /// allocation, no waker clone, and no user vtable indirection beyond the one
    /// [`RawWaker`] call — the [`TaskId`]-direct hot path Stage 1 asks for. A
    /// completion slab can register this waker and notify the task by index.
    ///
    /// Requires `&'static self` because a [`Waker`] erases lifetimes and may be
    /// cloned or moved anywhere; a Molt executor is a singleton `static`, so its
    /// handles live for the whole program and the erased pointer stays valid.
    pub fn waker(&'static self, task: TaskId) -> Waker {
        let handle = &self.handles[task.0 as usize];
        // SAFETY: `handle` points at a `Handle` owned by this `'static`
        // executor, so it satisfies the raw waker's validity contract, and
        // `raw_vtable::<N>` only ever dereferences such handles.
        unsafe {
            Waker::from_raw(RawWaker::new((handle as *const Handle).cast(), raw_vtable::<N>()))
        }
    }
}

/// The shared vtable for [`Executor::waker`], monomorphized per capacity so the
/// wake handlers can recover the correctly typed [`Executor`] from a handle.
fn raw_vtable<const N: usize>() -> &'static RawWakerVTable {
    &const { RawWakerVTable::new(clone_raw::<N>, wake_raw::<N>, wake_raw::<N>, drop_raw) }
}

/// Clones the waker by copying its handle pointer; the handle is `'static`.
///
/// # Safety
///
/// `data` must be a handle pointer produced by [`Executor::waker`].
unsafe fn clone_raw<const N: usize>(data: *const ()) -> RawWaker {
    RawWaker::new(data, raw_vtable::<N>())
}

/// Recovers the owning executor from the handle and marks its task ready.
///
/// # Safety
///
/// `data` must be a handle pointer produced by [`Executor::waker`].
unsafe fn wake_raw<const N: usize>(data: *const ()) {
    let handle = data as *const Handle;
    // SAFETY: `data` is a live handle pointer per this function's contract.
    let index = unsafe { (*handle).index };
    // The handle is `self.handles[index]`; step back to the array base, then to
    // the enclosing executor via the field offset.
    let handles_base = handle.wrapping_byte_sub(index as usize * size_of::<Handle>());
    let executor =
        handles_base.wrapping_byte_sub(offset_of!(Executor<N>, handles)) as *const Executor<N>;
    // SAFETY: the handle lives inside a `'static` executor, so reconstructing
    // that executor's pointer and borrowing it shared is sound.
    unsafe {
        (*executor).wake(TaskId(index));
    }
}

/// Drops the waker. The handle is borrowed from a `'static` executor, so there
/// is nothing to release.
fn drop_raw(_data: *const ()) {}

impl<const N: usize> Default for Executor<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::pin::pin;
    use core::task::{Context, Poll};
    use std::boxed::Box;
    use std::future::Future;

    use super::Executor;
    use crate::completion::CompletionSlab;

    /// Leaks an executor so it satisfies the `&'static self` waker contract, as
    /// the kernel's singleton executor would.
    fn static_executor<const N: usize>() -> &'static Executor<N> {
        Box::leak(Box::new(Executor::<N>::new()))
    }

    #[test]
    fn waker_marks_only_its_own_task_ready() {
        let executor = static_executor::<4>();
        let first = executor.register().expect("free slot");
        let second = executor.register().expect("free slot");

        executor.waker(second).wake();

        assert_eq!(executor.next_ready(), Some(second), "the woken task became ready");
        assert_eq!(executor.next_ready(), None, "no other task was disturbed");
        let _ = first;
    }

    #[test]
    fn cloned_waker_wakes_the_same_task() {
        let executor = static_executor::<2>();
        let task = executor.register().expect("free slot");

        // A clone copies only the handle pointer, so it must resolve back to the
        // same task through the same executor.
        let clone = executor.waker(task).clone();
        clone.wake();

        assert_eq!(executor.next_ready(), Some(task));
    }

    #[test]
    fn completion_wakes_the_registered_executor_task() {
        // End-to-end: a completion fired from the "driver" side wakes the task
        // through the slab's stored waker, which is the executor-native waker,
        // so the executor observes the task as ready with no polling loop.
        let executor = static_executor::<2>();
        let task = executor.register().expect("free slot");

        let slab = CompletionSlab::<u32, 2>::new();
        let token = slab.reserve().expect("free completion slot");
        let mut future = pin!(slab.wait(token));

        let waker = executor.waker(task);
        let mut context = Context::from_waker(&waker);
        assert_eq!(future.as_mut().poll(&mut context), Poll::Pending, "no result yet");
        assert_eq!(executor.next_ready(), None, "still parked before completion");

        slab.complete(token.request_id(), 42).expect("live request id");
        assert_eq!(executor.next_ready(), Some(task), "completion woke the task by id");

        executor.complete_poll(task);
        assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Ok(42)));
    }
}
