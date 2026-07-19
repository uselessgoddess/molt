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
//! returned waker points straight at the task's own state word rather than at
//! an owned, cloned [`Waker`]: waking is one bit-set on that word, cloning
//! copies one pointer, and dropping is a no-op. That keeps the completion hot
//! path free of allocation, waker copies, and any user vtable indirection
//! beyond the single unavoidable [`RawWaker`] call, which is the [`TaskId`]-
//! direct integration Stage 1 calls for.
//!
//! Pointing at the state word rather than at the executor also keeps the waker
//! within the provenance it was built from: it only ever touches the one
//! [`AtomicU8`] it was handed, so no pointer arithmetic escapes the slot it
//! borrows, and the vtable needs no capacity parameter.

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

/// A bounded task registry and ready queue represented by atomic slot states.
pub struct Executor<const N: usize> {
    states: [AtomicU8; N],
}

impl<const N: usize> Executor<N> {
    pub const fn new() -> Self {
        const { assert!(N > 0 && N <= 256, "executor capacity must be in 1..=256") };
        Self { states: [const { AtomicU8::new(0) }; N] }
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
        if let Some(state) = self.states.get(task.0 as usize) {
            set_ready(state);
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
    /// slot states live for the whole program and the erased pointer stays
    /// valid.
    pub fn waker(&'static self, task: TaskId) -> Waker {
        let state: &'static AtomicU8 = &self.states[task.0 as usize];
        // SAFETY: `state` borrows one slot of a `'static` executor, so it stays
        // live and shared-borrowable forever, which is what `VTABLE` requires.
        unsafe { Waker::from_raw(RawWaker::new((state as *const AtomicU8).cast(), &VTABLE)) }
    }
}

/// Sets the ready bit on an occupied slot, leaving free slots untouched.
fn set_ready(state: &AtomicU8) {
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

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_raw, drop_raw);

/// # Safety
///
/// `data` must be a slot-state pointer produced by [`Executor::waker`].
unsafe fn clone_raw(data: *const ()) -> RawWaker {
    RawWaker::new(data, &VTABLE)
}

/// # Safety
///
/// `data` must be a slot-state pointer produced by [`Executor::waker`].
unsafe fn wake_raw(data: *const ()) {
    // SAFETY: per this function's contract `data` borrows a slot of a `'static`
    // executor, so it is live and safe to borrow shared.
    set_ready(unsafe { &*data.cast::<AtomicU8>() });
}

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
    use std::future::Future;

    use super::Executor;
    use crate::completion::CompletionSlab;

    // `waker` takes `&'static self`, matching the singleton executor a kernel
    // actually owns. Each test declares its own static rather than leaking a
    // box, so nothing outlives the test binary unaccounted for.

    #[test]
    fn waker_marks_only_its_own_task_ready() {
        static EXECUTOR: Executor<4> = Executor::new();
        let executor = &EXECUTOR;
        let first = executor.register().expect("free slot");
        let second = executor.register().expect("free slot");

        executor.waker(second).wake();

        assert_eq!(executor.next_ready(), Some(second), "the woken task became ready");
        assert_eq!(executor.next_ready(), None, "no other task was disturbed");
        let _ = first;
    }

    #[test]
    fn cloned_waker_wakes_the_same_task() {
        static EXECUTOR: Executor<2> = Executor::new();
        let executor = &EXECUTOR;
        let task = executor.register().expect("free slot");

        let clone = executor.waker(task).clone();
        clone.wake();

        assert_eq!(executor.next_ready(), Some(task));
    }

    #[test]
    fn completion_wakes_the_registered_executor_task() {
        static EXECUTOR: Executor<2> = Executor::new();
        let executor = &EXECUTOR;
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
