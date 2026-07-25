//! One [`Heap`] behind a lock, shaped for `#[global_allocator]`.
//!
//! The lock is a plain spin flag: Molt runs one core, and the kernel never
//! allocates from an interrupt, so there is nothing yet for a fairer lock to
//! buy. An SMP kernel replaces the flag, not the heap under it.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::heap::Heap;

/// A shared heap that satisfies `alloc`.
pub struct Global {
    taken: AtomicBool,
    heap: UnsafeCell<Heap>,
}

// SAFETY: the heap is reachable only through `lock`, which hands it to one
// caller at a time, and `Heap` itself is `Send`.
unsafe impl Sync for Global {}

impl Global {
    pub const fn new() -> Self {
        Self { taken: AtomicBool::new(false), heap: UnsafeCell::new(Heap::new()) }
    }

    /// Donates the `len` bytes at `start` to the heap.
    ///
    /// # Safety
    ///
    /// Same obligation as [`Heap::extend`]: the span is writable, unaliased,
    /// and lives as long as the allocator does.
    pub unsafe fn extend(&self, start: *mut u8, len: usize) {
        // SAFETY: the caller carries `Heap::extend`'s obligation forward.
        unsafe { self.lock().extend(start, len) };
    }

    /// Bytes donated to the heap.
    pub fn size(&self) -> usize {
        self.lock().size()
    }

    /// Bytes held by live allocations.
    pub fn used(&self) -> usize {
        self.lock().used()
    }

    fn lock(&self) -> Guard<'_> {
        while self.taken.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Guard(self)
    }
}

impl Default for Global {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every allocation comes from the heap and is returned to it, and the
// lock keeps the list consistent across callers.
unsafe impl GlobalAlloc for Global {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.lock().allocate(layout).map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };
        // SAFETY: `alloc` returned `ptr` from this heap, which the caller of
        // `dealloc` promises has not been released since.
        unsafe { self.lock().deallocate(ptr) };
    }
}

/// Releases the flag when the borrow ends.
struct Guard<'g>(&'g Global);

impl Deref for Guard<'_> {
    type Target = Heap;

    fn deref(&self) -> &Heap {
        // SAFETY: holding the guard means holding the flag, so no other
        // reference to the heap exists.
        unsafe { &*self.0.heap.get() }
    }
}

impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Heap {
        // SAFETY: holding the guard means holding the flag, so no other
        // reference to the heap exists.
        unsafe { &mut *self.0.heap.get() }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.taken.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::{GlobalAlloc, Layout};
    use std::vec;

    use super::Global;

    #[test]
    fn donated_span_serves_allocations() {
        let mut bytes = vec![0u8; 4096];
        let allocator = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };

        let layout = Layout::from_size_align(64, 16).unwrap();
        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout) };

        assert!(!live.is_null());
        assert!(allocator.used() >= 64);
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout) };
        assert_eq!(allocator.used(), 0);
    }

    #[test]
    fn empty_heap_refuses() {
        let allocator = Global::new();

        // SAFETY: a null return is the documented refusal, freed by nobody.
        let refused = unsafe { allocator.alloc(Layout::from_size_align(8, 8).unwrap()) };

        assert!(refused.is_null());
    }
}
