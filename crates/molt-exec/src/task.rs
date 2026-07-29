//! A future, a place in a queue, and a state two cores can both see.
//!
//! The allocation is an `Arc` because a waker outlives the poll that made it,
//! and the future inside is touched only by the core that owns the task: the
//! owner's task table holds a reference until the future is dropped, so a
//! waker released elsewhere frees memory and never a future. That is what lets
//! a task that is not `Send` be woken by a core it could not run on.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{Context, RawWaker, RawWakerVTable, Waker};

use crate::exec::Shared;

/// How soon a core gets to a task, and nothing more.
///
/// A level is a queue, not a deadline: a high task runs before a normal one
/// that is also ready, and never instead of it — see the slices in `exec`.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum Priority {
    /// What a device is waiting on.
    High,
    #[default]
    Normal,
    /// What an idle core has time for.
    Low,
}

impl Priority {
    pub const LEVELS: usize = 3;

    pub(crate) const ALL: [Self; Self::LEVELS] = [Self::High, Self::Normal, Self::Low];

    pub(crate) const fn level(self) -> usize {
        self as usize
    }
}

/// Queued, and nobody else's to queue.
const SCHEDULED: u8 = 1 << 0;
/// Being polled, so a wake leaves a note instead of a queue entry.
const RUNNING: u8 = 1 << 1;
/// Woken mid-poll: the poller queues it again on the way out.
const WOKEN: u8 = 1 << 2;
/// The future is gone. Nothing schedules it after this.
const DONE: u8 = 1 << 3;
/// In the owner's task table, at [`Task::slot`].
const HELD: u8 = 1 << 4;

pub(crate) struct Task {
    state: AtomicU8,
    priority: Priority,
    /// Where the owner's table keeps it, so finishing is a swap and not a scan.
    slot: Cell<usize>,
    /// The inbox link, written by whoever queued it and cleared by the drain.
    next: UnsafeCell<*const Task>,
    future: UnsafeCell<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    shared: Arc<Shared>,
}

impl Task {
    /// Deliberately not `Send`: a reference travels, the future does not. What
    /// a far core can drop is the count, and by then the future is long gone —
    /// the owner cleared it before letting the table's reference go.
    #[allow(clippy::arc_with_non_send_sync)]
    pub(crate) fn new(
        shared: Arc<Shared>,
        priority: Priority,
        future: Pin<Box<dyn Future<Output = ()>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(0),
            priority,
            slot: Cell::new(0),
            next: UnsafeCell::new(ptr::null()),
            future: UnsafeCell::new(Some(future)),
            shared,
        })
    }

    pub(crate) fn priority(&self) -> Priority {
        self.priority
    }

    /// Puts the task in its core's inbox, unless it is already there or running.
    pub(crate) fn schedule(self: &Arc<Self>) {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & (DONE | SCHEDULED) != 0 {
                return;
            }
            let next = if state & RUNNING != 0 { state | WOKEN } else { state | SCHEDULED };
            match self.state.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    if next & SCHEDULED != 0 {
                        self.shared.queue(self.clone());
                    }
                    return;
                }
                Err(actual) => state = actual,
            }
        }
    }

    /// Polls the future once, and reports whether the task is finished with.
    pub(crate) fn run(self: &Arc<Self>) -> bool {
        // The queued flag is spent before the poll, not after: a wake arriving
        // mid-poll has to be able to queue the task again.
        let taken = self.update(|state| (state & !SCHEDULED) | RUNNING);
        if taken & DONE != 0 {
            return true;
        }

        let waker = self.waker();
        let mut context = Context::from_waker(&waker);
        // SAFETY: the owning core polls its tasks one at a time, and a task
        // being polled is in no queue for a second poll to come from.
        let future = unsafe { &mut *self.future.get() };
        let done = match future.as_mut() {
            Some(future) => future.as_mut().poll(&mut context).is_ready(),
            None => true,
        };
        if done {
            *future = None;
            self.update(|state| (state & !(RUNNING | WOKEN)) | DONE);
            return true;
        }

        let before = self.update(|state| {
            if state & WOKEN != 0 {
                (state & !(RUNNING | WOKEN)) | SCHEDULED
            } else {
                state & !RUNNING
            }
        });
        if before & WOKEN != 0 {
            self.shared.queue(self.clone());
        }
        false
    }

    /// Drops the future here, which is the core that owns it.
    pub(crate) fn finish(&self) {
        self.update(|state| (state & !(RUNNING | WOKEN | SCHEDULED)) | DONE);
        // SAFETY: the owner is the one calling this, and it is not polling.
        unsafe { *self.future.get() = None };
    }

    pub(crate) fn held(&self) -> bool {
        self.state.load(Ordering::Relaxed) & HELD != 0
    }

    pub(crate) fn hold(&self, slot: usize) {
        self.slot.set(slot);
        self.state.fetch_or(HELD, Ordering::Relaxed);
    }

    pub(crate) fn release(&self) {
        self.state.fetch_and(!HELD, Ordering::Relaxed);
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot.get()
    }

    pub(crate) fn moved(&self, slot: usize) {
        self.slot.set(slot);
    }

    /// Applies `f` until it takes, and reports what it replaced.
    fn update(&self, f: impl Fn(u8) -> u8) -> u8 {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            match self.state.compare_exchange_weak(
                state,
                f(state),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) => return previous,
                Err(actual) => state = actual,
            }
        }
    }

    /// # Safety
    ///
    /// Only the core queuing the task may write its link, and only until the
    /// push that hands it over lands.
    pub(crate) unsafe fn link(&self, next: *const Task) {
        // SAFETY: the caller holds the task and nothing else can reach it.
        unsafe { *self.next.get() = next };
    }

    /// # Safety
    ///
    /// Only the drain that took the task off the inbox may read its link.
    pub(crate) unsafe fn unlink(&self) -> *const Task {
        // SAFETY: the drain owns the whole chain it is walking.
        unsafe { self.next.get().replace(ptr::null()) }
    }

    fn waker(self: &Arc<Self>) -> Waker {
        let raw = Arc::into_raw(self.clone());
        // SAFETY: a leaked strong reference, which is what every arm of
        // `VTABLE` is written to expect.
        unsafe { Waker::from_raw(RawWaker::new(raw.cast(), &VTABLE)) }
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, discard);

/// # Safety
///
/// `data` must be a strong reference to a [`Task`], as [`Task::waker`] makes.
unsafe fn clone(data: *const ()) -> RawWaker {
    // SAFETY: the contract gives a live task, so the count is live too.
    unsafe { Arc::increment_strong_count(data.cast::<Task>()) };
    RawWaker::new(data, &VTABLE)
}

/// # Safety
///
/// `data` must be a strong reference to a [`Task`], as [`Task::waker`] makes.
unsafe fn wake(data: *const ()) {
    // SAFETY: the contract hands this reference over.
    let task = unsafe { Arc::from_raw(data.cast::<Task>()) };
    task.schedule();
}

/// # Safety
///
/// `data` must be a strong reference to a [`Task`], as [`Task::waker`] makes.
unsafe fn wake_by_ref(data: *const ()) {
    // SAFETY: the contract lends this reference, so the count stays as it was.
    let task = ManuallyDrop::new(unsafe { Arc::from_raw(data.cast::<Task>()) });
    task.schedule();
}

/// # Safety
///
/// `data` must be a strong reference to a [`Task`], as [`Task::waker`] makes.
unsafe fn discard(data: *const ()) {
    // SAFETY: the contract hands this reference over.
    drop(unsafe { Arc::from_raw(data.cast::<Task>()) });
}
