//! One [`Heap`] behind a lock, shaped for `#[global_allocator]`.
//!
//! The lock is a plain spin flag: Molt runs one core, and the kernel never
//! allocates from an interrupt, so there is nothing yet for a fairer lock to
//! buy. An SMP kernel replaces the flag, not the heap under it.
//!
//! That "never" is not a hope. An interrupt path opens with
//! [`Global::interrupt`], and the heap refuses every request made while that
//! guard is held — see it for why the alternative is a machine that stops.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::heap::Heap;

/// A shared heap that satisfies `alloc`.
pub struct Global {
    taken: AtomicBool,
    serving: AtomicUsize,
    heap: UnsafeCell<Heap>,
}

// SAFETY: the heap is reachable only through `lock`, which hands it to one
// caller at a time, and `Heap` itself is `Send`.
unsafe impl Sync for Global {}

impl Global {
    pub const fn new() -> Self {
        Self {
            taken: AtomicBool::new(false),
            serving: AtomicUsize::new(0),
            heap: UnsafeCell::new(Heap::new()),
        }
    }

    /// Bars the heap for as long as an interrupt is being serviced.
    ///
    /// An interrupt preempts whoever was running, and the lock below is a spin
    /// flag: if the interrupted code held it, an allocation from the handler
    /// spins for a flag only that code can release, on a core that will never
    /// run it again. The kernel therefore does not allocate from an interrupt,
    /// and this is what makes that the allocator's business rather than a
    /// comment — an interrupt path opens with this guard, and a request made
    /// under it is refused: null out of [`GlobalAlloc::alloc`], which the
    /// caller already handles as an empty heap. A free is dropped for the same
    /// reason, because leaking a block is recoverable and wedging a core is
    /// not. Debug builds trip the assert first: the refusal is a bug in the
    /// handler, not a shortage.
    ///
    /// Nested interrupts nest guards, and the heap opens again when the
    /// outermost one ends.
    pub fn interrupt(&self) -> Interrupt<'_> {
        self.serving.fetch_add(1, Ordering::Acquire);
        Interrupt(self)
    }

    /// Whether an interrupt is being serviced, which bars the heap.
    pub fn interrupted(&self) -> bool {
        self.serving.load(Ordering::Acquire) != 0
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

    /// The heap, unless an interrupt is being serviced.
    fn reach(&self) -> Option<Guard<'_>> {
        debug_assert!(!self.interrupted(), "an interrupt asked the kernel heap for memory");
        (!self.interrupted()).then(|| self.lock())
    }
}

/// Keeps the heap barred until the interrupt it spans returns.
pub struct Interrupt<'g>(&'g Global);

impl Drop for Interrupt<'_> {
    fn drop(&mut self) {
        self.0.serving.fetch_sub(1, Ordering::Release);
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
        let Some(mut heap) = self.reach() else {
            return ptr::null_mut();
        };
        heap.allocate(layout).map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let (Some(ptr), Some(mut heap)) = (NonNull::new(ptr), self.reach()) else {
            return;
        };
        // SAFETY: `alloc` returned `ptr` from this heap, which the caller of
        // `dealloc` promises has not been released since.
        unsafe { heap.deallocate(ptr) };
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

    /// Debug builds say an interrupt asked at all; release builds refuse it.
    #[test]
    #[cfg_attr(debug_assertions, should_panic = "an interrupt asked the kernel heap for memory")]
    fn interrupt_refused() {
        let mut bytes = vec![0u8; 4096];
        let allocator = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };

        let _interrupt = allocator.interrupt();
        // SAFETY: a null return is the documented refusal, freed by nobody.
        let refused = unsafe { allocator.alloc(Layout::from_size_align(64, 16).unwrap()) };

        assert!(refused.is_null(), "the heap served an interrupt");
    }

    #[test]
    fn heap_opens_after_interrupt() {
        let mut bytes = vec![0u8; 4096];
        let allocator = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };
        let layout = Layout::from_size_align(64, 16).unwrap();

        drop(allocator.interrupt());
        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout) };

        assert!(!allocator.interrupted(), "the guard left the heap barred");
        assert!(!live.is_null(), "the heap stayed barred after the interrupt");
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout) };
    }

    #[test]
    fn empty_heap_refuses() {
        let allocator = Global::new();

        // SAFETY: a null return is the documented refusal, freed by nobody.
        let refused = unsafe { allocator.alloc(Layout::from_size_align(8, 8).unwrap()) };

        assert!(refused.is_null());
    }
}
