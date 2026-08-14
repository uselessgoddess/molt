//! Test doubles the models share, which is where the second `Arc` lives.
//!
//! In a test `Arc` unqualified is [`limen::Arc`], loom's under `--cfg loom`.
//! `Wake` takes the real `alloc::sync::Arc` and nothing else, so that one is
//! reached here and nowhere else, where the two cannot be confused.

use alloc::sync::Arc;
use alloc::task::Wake;

use limen::atomic::{AtomicBool, Ordering};

/// A waker that remembers being woken, so a test can ask whether it was.
pub(crate) struct Flag(AtomicBool);

impl Flag {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(false)))
    }

    pub(crate) fn fired(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}
