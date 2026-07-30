//! The one thing two cores touch: a stack anyone pushes to and its owner
//! empties.
//!
//! Pushing is a compare-and-swap on one word, so a core waking a task on
//! another core never waits on a lock it cannot see the holder of. Draining is
//! a single swap that takes the whole chain at once; the owner then reverses it
//! in place, which turns the stack back into the order the pushes arrived in.

use alloc::sync::Arc;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::task::Task;

pub(crate) struct Inbox {
    head: AtomicPtr<Task>,
}

impl Inbox {
    pub(crate) const fn new() -> Self {
        Self { head: AtomicPtr::new(ptr::null_mut()) }
    }

    pub(crate) fn push(&self, task: Arc<Task>) {
        let task = Arc::into_raw(task).cast_mut();
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: the task is ours until the swap below publishes it.
            unsafe { (*task).link(head) };
            match self.head.compare_exchange_weak(head, task, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }

    pub(crate) fn empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Takes everything pushed so far, oldest first.
    pub(crate) fn drain(&self) -> Drain {
        Drain { head: reverse(self.head.swap(ptr::null_mut(), Ordering::AcqRel)) }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        drop(self.drain());
    }
}

/// Flips the push order back into arrival order.
fn reverse(mut head: *mut Task) -> *mut Task {
    let mut done = ptr::null_mut();
    while !head.is_null() {
        // SAFETY: the chain came off the inbox, so it is the drain's alone.
        let next = unsafe { (*head).unlink() }.cast_mut();
        // SAFETY: the same, and the link is the one just cleared.
        unsafe { (*head).link(done) };
        done = head;
        head = next;
    }
    done
}

pub(crate) struct Drain {
    head: *mut Task,
}

impl Iterator for Drain {
    type Item = Arc<Task>;

    fn next(&mut self) -> Option<Self::Item> {
        let task = self.head;
        if task.is_null() {
            return None;
        }
        // SAFETY: the chain is the drain's, so its links are too.
        self.head = unsafe { (*task).unlink() }.cast_mut();
        // SAFETY: the strong reference `push` handed over.
        Some(unsafe { Arc::from_raw(task.cast_const()) })
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        while self.next().is_some() {}
    }
}
