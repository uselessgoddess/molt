//! Allocation-free bounded ready scheduling.

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskId(u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    Full,
}

/// A bounded task registry and ready queue represented by atomic bit sets.
pub struct Executor<const N: usize> {
    occupied: AtomicU64,
    ready: AtomicU64,
}

impl<const N: usize> Executor<N> {
    pub const fn new() -> Self {
        const { assert!(N > 0 && N <= 64, "executor capacity must be in 1..=64") };
        Self { occupied: AtomicU64::new(0), ready: AtomicU64::new(0) }
    }

    pub fn register(&self) -> Result<TaskId, SpawnError> {
        let mask = if N == 64 { u64::MAX } else { (1_u64 << N) - 1 };
        let mut occupied = self.occupied.load(Ordering::Acquire);
        loop {
            let free = !occupied & mask;
            if free == 0 {
                return Err(SpawnError::Full);
            }
            let bit = 1_u64 << free.trailing_zeros();
            match self.occupied.compare_exchange_weak(
                occupied,
                occupied | bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(TaskId(bit.trailing_zeros() as u8)),
                Err(actual) => occupied = actual,
            }
        }
    }

    pub fn unregister(&self, task: TaskId) {
        let bit = 1_u64 << task.0;
        self.ready.fetch_and(!bit, Ordering::AcqRel);
        self.occupied.fetch_and(!bit, Ordering::AcqRel);
    }

    pub fn wake(&self, task: TaskId) {
        let bit = 1_u64 << task.0;
        if self.occupied.load(Ordering::Acquire) & bit != 0 {
            self.ready.fetch_or(bit, Ordering::Release);
        }
    }

    pub fn next_ready(&self) -> Option<TaskId> {
        let mut ready = self.ready.load(Ordering::Acquire);
        loop {
            if ready == 0 {
                return None;
            }
            let bit = 1_u64 << ready.trailing_zeros();
            match self.ready.compare_exchange_weak(
                ready,
                ready & !bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(TaskId(bit.trailing_zeros() as u8)),
                Err(actual) => ready = actual,
            }
        }
    }
}

impl<const N: usize> Default for Executor<N> {
    fn default() -> Self {
        Self::new()
    }
}
