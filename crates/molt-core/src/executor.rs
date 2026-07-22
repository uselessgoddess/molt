//! Allocation-free bounded ready scheduling.
//!
//! Per-task atomics avoid contention between unrelated wakes; [`Padded`] trades
//! memory for cache-line isolation. [`Executor::waker`] points directly at a
//! pinned slot, making wake and clone allocation-free while preserving pointer
//! provenance. Slot addresses must remain stable for every outstanding waker.

use core::borrow::Borrow;
use core::task::{RawWaker, RawWakerVTable, Waker};

use crate::cache::{CacheLayout, CachePadded, Compact, Padded};
use crate::sync::atomic::{AtomicU8, Ordering};

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
pub struct Executor<const N: usize, L: CacheLayout = Compact> {
    states: [L::Slot<AtomicU8>; N],
}

#[cfg(not(loom))]
impl<const N: usize> Executor<N, Compact> {
    pub const fn new() -> Self {
        const { assert!(N > 0 && N <= 256, "executor capacity must be in 1..=256") };
        Self { states: [const { AtomicU8::new(0) }; N] }
    }
}

#[cfg(loom)]
impl<const N: usize> Executor<N, Compact> {
    pub fn new() -> Self {
        assert!(N > 0 && N <= 256, "executor capacity must be in 1..=256");
        Self { states: core::array::from_fn(|_| AtomicU8::new(0)) }
    }
}

#[cfg(not(loom))]
impl<const N: usize> Executor<N, Padded> {
    pub const fn new() -> Self {
        const { assert!(N > 0 && N <= 256, "executor capacity must be in 1..=256") };
        Self { states: [const { CachePadded::new(AtomicU8::new(0)) }; N] }
    }
}

#[cfg(loom)]
impl<const N: usize> Executor<N, Padded> {
    pub fn new() -> Self {
        assert!(N > 0 && N <= 256, "executor capacity must be in 1..=256");
        Self { states: core::array::from_fn(|_| CachePadded::new(AtomicU8::new(0))) }
    }
}

impl<const N: usize, L: CacheLayout> Executor<N, L> {
    pub fn register(&self) -> Result<TaskId, SpawnError> {
        for (index, state) in self.states.iter().enumerate() {
            let state = state.borrow();
            if state.compare_exchange(0, OCCUPIED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Ok(TaskId(index as u8));
            }
        }
        Err(SpawnError::Full)
    }

    pub fn unregister(&self, task: TaskId) {
        if let Some(state) = self.states.get(task.0 as usize) {
            let state = state.borrow();
            state.store(0, Ordering::Release);
        }
    }

    pub fn wake(&self, task: TaskId) {
        if let Some(state) = self.states.get(task.0 as usize) {
            set_ready(state.borrow());
        }
    }

    pub fn next_ready(&self) -> Option<TaskId> {
        for (index, state) in self.states.iter().enumerate() {
            let state = state.borrow();
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
            let state = state.borrow();
            state.fetch_and(!POLLING, Ordering::Release);
        }
    }

    /// Builds a [`Waker`] that marks `task` ready in this executor.
    ///
    /// The static receiver keeps the slot pointer valid after lifetime erasure.
    pub fn waker(&'static self, task: TaskId) -> Waker {
        let state: &'static AtomicU8 = self.states[task.0 as usize].borrow();
        // SAFETY: `state` is a shared slot in a static executor, as `VTABLE` requires.
        unsafe { Waker::from_raw(RawWaker::new((state as *const AtomicU8).cast(), &VTABLE)) }
    }
}

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
    // SAFETY: the contract guarantees a live, shared slot pointer.
    set_ready(unsafe { &*data.cast::<AtomicU8>() });
}

fn drop_raw(_data: *const ()) {}

impl<const N: usize> Default for Executor<N, Compact> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Default for Executor<N, Padded> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;

    use super::Executor;

    #[test]
    fn race_keeps_wake() {
        loom::model(|| {
            let executor = Arc::new(Executor::<2>::new());
            let task = executor.register().expect("free slot");

            let notifier = {
                let executor = executor.clone();
                thread::spawn(move || executor.wake(task))
            };
            let scanned = executor.next_ready();
            notifier.join().unwrap();

            assert!(
                scanned == Some(task) || executor.next_ready() == Some(task),
                "the wake was lost between the scan and the wake"
            );
        });
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    extern crate std;

    use core::mem::size_of;
    use core::pin::pin;
    use core::task::{Context, Poll};

    use super::Executor;
    use crate::cache::Padded;
    use crate::completion::CompletionSlab;

    #[test]
    fn waker_is_task_local() {
        static EXECUTOR: Executor<4> = Executor::<4>::new();
        let executor = &EXECUTOR;
        let first = executor.register().expect("free slot");
        let second = executor.register().expect("free slot");

        executor.waker(second).wake();

        assert_eq!(executor.next_ready(), Some(second), "the woken task became ready");
        assert_eq!(executor.next_ready(), None, "no other task was disturbed");
        let _ = first;
    }

    #[test]
    fn clone_keeps_task() {
        static EXECUTOR: Executor<2> = Executor::<2>::new();
        let executor = &EXECUTOR;
        let task = executor.register().expect("free slot");

        let clone = executor.waker(task).clone();
        clone.wake();

        assert_eq!(executor.next_ready(), Some(task));
    }

    #[test]
    fn completion_wakes_task() {
        static EXECUTOR: Executor<2> = Executor::<2>::new();
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

    #[test]
    fn padded_layout_schedules() {
        let executor = Executor::<2, Padded>::new();
        let task = executor.register().expect("free slot");

        executor.wake(task);

        assert_eq!(executor.next_ready(), Some(task));
    }

    #[test]
    fn layout_is_typed() {
        assert!(size_of::<Executor<4, Padded>>() > size_of::<Executor<4>>());
    }
}
