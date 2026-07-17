//! Bounded single-producer/single-consumer rings.
//!
//! The queue is split into non-cloneable endpoints. This makes the SPSC
//! contract a property of the safe API instead of a convention callers must
//! remember.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

/// A fixed-capacity single-producer/single-consumer queue.
pub struct SpscRing<T, const N: usize> {
    slots: [UnsafeCell<MaybeUninit<T>>; N],
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: safe construction yields exactly one producer and one consumer.
// A value crosses threads only when `T: Send`, with release/acquire publication.
unsafe impl<T: Send, const N: usize> Sync for SpscRing<T, N> {}

impl<T, const N: usize> SpscRing<T, N> {
    /// Creates an empty queue.
    pub const fn new() -> Self {
        const {
            assert!(N > 0, "a ring must contain at least one slot");
        }

        Self {
            slots: [const { UnsafeCell::new(MaybeUninit::uninit()) }; N],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Returns the number of entries the queue can hold.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Divides the queue into its unique producer and consumer endpoints.
    pub fn split(&mut self) -> (Producer<'_, T, N>, Consumer<'_, T, N>) {
        (Producer { ring: self }, Consumer { ring: self })
    }

    fn try_push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) == N {
            return Err(value);
        }

        // SAFETY: only the unique producer writes this slot. The acquire load
        // above observes the consumer's release before a wrapped slot is reused.
        unsafe {
            (*self.slots[tail % N].get()).write(value);
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    fn try_pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }

        // SAFETY: the producer's release published a fully initialized value,
        // and only the unique consumer reads and advances this slot.
        let value = unsafe { (*self.slots[head % N].get()).assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

impl<T, const N: usize> Default for SpscRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for SpscRing<T, N> {
    fn drop(&mut self) {
        let mut head = *self.head.get_mut();
        let tail = *self.tail.get_mut();
        while head != tail {
            // SAFETY: exclusive access prevents concurrent endpoints, and each
            // position between head and tail contains one initialized value.
            unsafe {
                self.slots[head % N].get_mut().assume_init_drop();
            }
            head = head.wrapping_add(1);
        }
    }
}

/// The unique submitting endpoint of an [`SpscRing`].
pub struct Producer<'ring, T, const N: usize> {
    ring: &'ring SpscRing<T, N>,
}

impl<T, const N: usize> Producer<'_, T, N> {
    /// Enqueues `value`, returning it unchanged when the queue is full.
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        self.ring.try_push(value)
    }
}

/// The unique receiving endpoint of an [`SpscRing`].
pub struct Consumer<'ring, T, const N: usize> {
    ring: &'ring SpscRing<T, N>,
}

impl<T, const N: usize> Consumer<'_, T, N> {
    /// Removes the oldest value, or returns `None` when the queue is empty.
    pub fn try_pop(&mut self) -> Option<T> {
        self.ring.try_pop()
    }
}

/// Stable correlation token copied from a submission to its completion.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One operation placed on an [`IoRing`]'s submission queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Submission<Op> {
    id: RequestId,
    op: Op,
}

impl<Op> Submission<Op> {
    pub const fn new(id: RequestId, op: Op) -> Self {
        Self { id, op }
    }

    pub const fn id(&self) -> RequestId {
        self.id
    }

    pub const fn operation(&self) -> &Op {
        &self.op
    }

    pub fn into_operation(self) -> Op {
        self.op
    }
}

/// One result placed on an [`IoRing`]'s completion queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completion<C> {
    id: RequestId,
    result: C,
}

impl<C> Completion<C> {
    pub const fn new(id: RequestId, result: C) -> Self {
        Self { id, result }
    }

    pub const fn id(&self) -> RequestId {
        self.id
    }

    pub const fn result(&self) -> &C {
        &self.result
    }

    pub fn into_result(self) -> C {
        self.result
    }
}

/// Paired submission and completion queues, following the `io_uring` model.
pub struct IoRing<Op, C, const N: usize> {
    submissions: SpscRing<Submission<Op>, N>,
    completions: SpscRing<Completion<C>, N>,
}

impl<Op, C, const N: usize> IoRing<Op, C, N> {
    pub const fn new() -> Self {
        Self { submissions: SpscRing::new(), completions: SpscRing::new() }
    }

    /// Creates the unique client and driver views of both queues.
    pub fn split(&mut self) -> (IoClient<'_, Op, C, N>, IoDriver<'_, Op, C, N>) {
        let (submission_tx, submission_rx) = self.submissions.split();
        let (completion_tx, completion_rx) = self.completions.split();
        (
            IoClient { submissions: submission_tx, completions: completion_rx },
            IoDriver { submissions: submission_rx, completions: completion_tx },
        )
    }
}

impl<Op, C, const N: usize> Default for IoRing<Op, C, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Client-side submission producer and completion consumer.
pub struct IoClient<'ring, Op, C, const N: usize> {
    submissions: Producer<'ring, Submission<Op>, N>,
    completions: Consumer<'ring, Completion<C>, N>,
}

impl<Op, C, const N: usize> IoClient<'_, Op, C, N> {
    pub fn try_submit(&mut self, submission: Submission<Op>) -> Result<(), Submission<Op>> {
        self.submissions.try_push(submission)
    }

    pub fn try_completion(&mut self) -> Option<Completion<C>> {
        self.completions.try_pop()
    }
}

/// Driver-side submission consumer and completion producer.
pub struct IoDriver<'ring, Op, C, const N: usize> {
    submissions: Consumer<'ring, Submission<Op>, N>,
    completions: Producer<'ring, Completion<C>, N>,
}

impl<Op, C, const N: usize> IoDriver<'_, Op, C, N> {
    pub fn try_next(&mut self) -> Option<Submission<Op>> {
        self.submissions.try_pop()
    }

    pub fn try_complete(&mut self, completion: Completion<C>) -> Result<(), Completion<C>> {
        self.completions.try_push(completion)
    }
}
