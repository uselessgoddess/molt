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

use core::sync::atomic::{AtomicU8, Ordering};

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
}

impl<const N: usize> Default for Executor<N> {
    fn default() -> Self {
        Self::new()
    }
}
