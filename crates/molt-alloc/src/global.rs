//! Sharded [`Heap`]s, shaped for `#[global_allocator]`.
//!
//! One heap per core, picked by a [`Router`], so two cores allocating at once
//! wait on nothing. A release names the heap it came out of, and one that is
//! not this core's is pushed onto the owner's stack instead of taken under its
//! lock: a store, which is why it is also what an interrupt does.
//!
//! An interrupt path opens with [`Global::interrupt`], and the heap it opened
//! on refuses every request made while that guard is held — see it for why the
//! alternative is a machine that stops. Only that one heap is barred; the other
//! cores never notice.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use crate::heap::{self, Heap};

/// Which heap the calling core allocates out of.
pub trait Router {
    fn shard() -> usize;
}

/// The routing of a kernel that has not brought its cores up.
pub struct Solo;

impl Router for Solo {
    fn shard() -> usize {
        0
    }
}

/// Heaps that satisfy `alloc`, one per shard `R` routes to.
pub struct Global<R: Router = Solo, const S: usize = 1> {
    shards: [Shard; S],
    next: AtomicUsize,
    router: PhantomData<fn() -> R>,
}

// SAFETY: a heap is reachable only through its shard's lock, and the stack of
// remote releases is atomic.
unsafe impl<R: Router, const S: usize> Sync for Global<R, S> {}

impl<R: Router, const S: usize> Global<R, S> {
    pub const fn new() -> Self {
        Self { shards: [const { Shard::new() }; S], next: AtomicUsize::new(0), router: PhantomData }
    }

    /// Bars this core's heap for as long as an interrupt is being serviced.
    pub fn interrupt(&self) -> Interrupt<'_> {
        let shard = self.here();
        shard.serving.fetch_add(1, Ordering::Acquire);
        Interrupt(shard)
    }

    /// Whether an interrupt is being serviced on this core, which bars its heap.
    pub fn interrupted(&self) -> bool {
        self.here().barred()
    }

    /// Donates the `len` bytes at `start` to one heap, taking each in turn.
    ///
    /// Whole rather than split: a span kept whole is a span one allocation can
    /// still claim whole, and a kernel donating what it claimed frame by frame
    /// reaches every heap anyway. What a heap runs out of, [`alloc`] borrows.
    ///
    /// [`alloc`]: GlobalAlloc::alloc
    ///
    /// # Safety
    ///
    /// Same obligation as [`Heap::extend`]: the span is writable, unaliased,
    /// and lives as long as the allocator does.
    pub unsafe fn extend(&self, start: *mut u8, len: usize) {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % S;
        let mut heap = self.shards[index].lock();
        heap.own(index);
        // SAFETY: the caller carries `Heap::extend`'s obligation forward.
        unsafe { heap.extend(start, len) };
    }

    /// Bytes donated to the heaps.
    pub fn size(&self) -> usize {
        self.shards.iter().map(|shard| shard.lock().size()).sum()
    }

    /// Bytes held by live allocations.
    pub fn used(&self) -> usize {
        self.shards.iter().map(|shard| shard.lock().used()).sum()
    }

    /// This core's heap.
    fn here(&self) -> &Shard {
        &self.shards[Self::index()]
    }

    fn index() -> usize {
        let shard = R::shard();
        debug_assert!(shard < S, "a core with no heap of its own");
        shard % S
    }
}

/// Keeps a heap barred until the interrupt it spans returns.
pub struct Interrupt<'g>(&'g Shard);

impl Drop for Interrupt<'_> {
    fn drop(&mut self) {
        self.0.serving.fetch_sub(1, Ordering::Release);
    }
}

impl<R: Router, const S: usize> Default for Global<R, S> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every allocation comes from a heap and is returned to the one that
// carved it, which its header names.
unsafe impl<R: Router, const S: usize> GlobalAlloc for Global<R, S> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let here = Self::index();
        debug_assert!(!self.shards[here].barred(), "an interrupt asked the kernel heap for memory");
        if self.shards[here].barred() {
            return ptr::null_mut();
        }
        // A core whose heap is spent borrows from the next one rather than
        // failing while bytes sit unused a shard over.
        for step in 0..S {
            if let Some(ptr) = self.shards[(here + step) % S].lock().allocate(layout) {
                return ptr.as_ptr();
            }
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };
        // SAFETY: `alloc` returned `ptr`, and what it returns names its heap.
        let owner = unsafe { heap::owner(ptr) };
        let shard = &self.shards[owner];
        // SAFETY: the header named this shard's heap as the one that carved it.
        unsafe {
            if owner == Self::index() && !shard.barred() {
                shard.lock().deallocate(ptr);
            } else {
                shard.push(ptr);
            }
        }
    }
}

/// One heap, the flag that hands it out, and what other cores gave back to it.
struct Shard {
    taken: AtomicBool,
    serving: AtomicUsize,
    heap: UnsafeCell<Heap>,
    remote: AtomicPtr<u8>,
}

impl Shard {
    const fn new() -> Self {
        Self {
            taken: AtomicBool::new(false),
            serving: AtomicUsize::new(0),
            heap: UnsafeCell::new(Heap::new()),
            remote: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Whether an interrupt is being serviced, which bars the heap.
    fn barred(&self) -> bool {
        self.serving.load(Ordering::Acquire) != 0
    }

    /// Takes the heap, first returning to it what was released from elsewhere.
    fn lock(&self) -> Guard<'_> {
        while self.taken.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        let mut guard = Guard(self);
        let mut node = self.remote.swap(ptr::null_mut(), Ordering::Acquire);
        while let Some(ptr) = NonNull::new(node) {
            // SAFETY: only this heap's allocations reach its stack, and the
            // link sits in a payload its owner had already given up.
            unsafe {
                node = ptr.cast::<*mut u8>().read();
                guard.deallocate(ptr);
            }
        }
        guard
    }

    /// Queues a release for the owning core, without waiting on its lock.
    ///
    /// The whole push is one CAS on the head and one write into the payload,
    /// so a core that never runs again strands the chunk and nothing else. A
    /// pop takes the stack entire, which is what keeps the head from being
    /// reused under a reader.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live allocation this shard's heap carved.
    unsafe fn push(&self, ptr: NonNull<u8>) {
        let mut head = self.remote.load(Ordering::Relaxed);
        loop {
            // SAFETY: a payload is unit-aligned and at least a unit wide, and
            // the caller has given it up.
            unsafe { ptr.cast::<*mut u8>().write(head) };
            let swapped = self.remote.compare_exchange_weak(
                head,
                ptr.as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            );
            match swapped {
                Ok(_) => return,
                Err(seen) => head = seen,
            }
        }
    }
}

struct Guard<'s>(&'s Shard);

impl Deref for Guard<'_> {
    type Target = Heap;

    fn deref(&self) -> &Heap {
        // SAFETY: holding the guard means holding the flag.
        unsafe { &*self.0.heap.get() }
    }
}

impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Heap {
        // SAFETY: holding the guard means holding the flag.
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
    use core::cell::Cell;
    use std::vec;

    use super::{Global, Router};

    std::thread_local! {
        /// The shard the running test allocates out of, standing in for a core.
        static SHARD: Cell<usize> = const { Cell::new(0) };
    }

    struct Pinned;

    impl Router for Pinned {
        fn shard() -> usize {
            SHARD.with(Cell::get)
        }
    }

    fn pin(shard: usize) {
        SHARD.with(|cell| cell.set(shard));
    }

    fn layout() -> Layout {
        Layout::from_size_align(64, 16).unwrap()
    }

    #[test]
    fn donated_span_serves_allocations() {
        let mut bytes = vec![0u8; 4096];
        let allocator: Global = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };

        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout()) };

        assert!(!live.is_null());
        assert!(allocator.used() >= 64);
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout()) };
        assert_eq!(allocator.used(), 0);
    }

    /// Debug builds say an interrupt asked at all; release builds refuse it.
    #[test]
    #[cfg_attr(debug_assertions, should_panic = "an interrupt asked the kernel heap for memory")]
    fn interrupt_refused() {
        let mut bytes = vec![0u8; 4096];
        let allocator: Global = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };

        let _interrupt = allocator.interrupt();
        // SAFETY: a null return is the documented refusal, freed by nobody.
        let refused = unsafe { allocator.alloc(layout()) };

        assert!(refused.is_null(), "the heap served an interrupt");
    }

    #[test]
    fn heap_opens_after_interrupt() {
        let mut bytes = vec![0u8; 4096];
        let allocator: Global = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };

        drop(allocator.interrupt());
        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout()) };

        assert!(!allocator.interrupted(), "the guard left the heap barred");
        assert!(!live.is_null(), "the heap stayed barred after the interrupt");
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout()) };
    }

    #[test]
    fn empty_heap_refuses() {
        let allocator: Global = Global::new();

        // SAFETY: a null return is the documented refusal, freed by nobody.
        let refused = unsafe { allocator.alloc(Layout::from_size_align(8, 8).unwrap()) };

        assert!(refused.is_null());
    }

    /// Two heaps over one vector, a donation each.
    fn pair(bytes: &mut [u8]) -> Global<Pinned, 2> {
        let allocator = Global::new();
        let (first, second) = bytes.split_at_mut(bytes.len() / 2);
        // SAFETY: the vector outlives the allocator, the halves are disjoint,
        // and nothing else touches either.
        unsafe {
            allocator.extend(first.as_mut_ptr(), first.len());
            allocator.extend(second.as_mut_ptr(), second.len());
        }
        allocator
    }

    #[test]
    fn remote_release_reaches_owner() {
        let mut bytes = vec![0u8; 8192];
        let allocator = pair(&mut bytes);

        pin(0);
        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout()) };
        pin(1);
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout()) };

        assert_eq!(allocator.used(), 0, "a release elsewhere never reached its heap");
    }

    #[test]
    fn interrupt_bars_one_heap() {
        let mut bytes = vec![0u8; 8192];
        let allocator = pair(&mut bytes);

        pin(0);
        let interrupt = allocator.interrupt();
        pin(1);
        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout()) };

        assert!(!live.is_null(), "one barred heap barred the rest");
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout()) };
        drop(interrupt);
    }

    #[test]
    fn spent_heap_borrows() {
        let mut bytes = vec![0u8; 4096];
        let allocator: Global<Pinned, 2> = Global::new();
        // SAFETY: the vector outlives the allocator and nothing else touches it.
        unsafe { allocator.extend(bytes.as_mut_ptr(), bytes.len()) };

        pin(1);
        // SAFETY: the layout is non-zero and the pointer is freed once.
        let live = unsafe { allocator.alloc(layout()) };

        assert!(!live.is_null(), "a core with no bytes of its own took none");
        // SAFETY: `live` came from this allocator with the same layout.
        unsafe { allocator.dealloc(live, layout()) };
        assert_eq!(allocator.used(), 0, "a borrowed chunk never went home");
    }
}
