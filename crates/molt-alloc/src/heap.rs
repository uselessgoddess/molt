//! Free-list heap over spans of donated bytes.
//!
//! Chunks are `UNIT`-aligned and `UNIT`-sized multiples, so a split never
//! leaves an unusable remainder alignment-wise. The free list is ordered by
//! address, which is what makes coalescing on release a constant-cost merge
//! with the two neighbours instead of a sweep.

use core::alloc::Layout;
use core::ptr::{NonNull, null_mut};

/// Chunk granularity, and the header every chunk carries.
const UNIT: usize = size_of::<Free>();

/// Bits of `back` that name whose heap a chunk came out of.
///
/// Chunk starts and payloads are both `UNIT`-aligned, so the distance between
/// them never uses them, and a release on another core needs no table to find
/// the owner — the allocation says so itself.
const TAG: usize = UNIT - 1;

/// Smallest chunk worth keeping: a header plus one unit of payload. A shorter
/// remainder stays with the allocation it was split from rather than becoming
/// a fragment nothing fits in.
const MIN: usize = 2 * UNIT;

const _: () = assert!(size_of::<Live>() == UNIT && align_of::<Live>() <= UNIT);

/// Header of a free chunk, stored in the chunk itself.
#[repr(C)]
struct Free {
    size: usize,
    next: *mut Free,
}

/// Header stored immediately before a live payload. `back` reaches the chunk
/// start, which alignment padding may have pushed further away than `UNIT`.
#[repr(C)]
struct Live {
    size: usize,
    back: usize,
}

/// A first-fit heap over the spans it was given.
pub struct Heap {
    free: *mut Free,
    size: usize,
    used: usize,
    owner: usize,
}

// SAFETY: the heap reaches its chunks only through `&mut self`, and the spans
// behind the list head belong to it alone, so no pointer here is thread-bound.
unsafe impl Send for Heap {}

impl Heap {
    pub const fn new() -> Self {
        Self { free: null_mut(), size: 0, used: 0, owner: 0 }
    }

    /// Stamps what this heap hands out, so a release anywhere names it.
    pub fn own(&mut self, owner: usize) {
        debug_assert!(owner <= TAG, "more heaps than a header can name");
        self.owner = owner & TAG;
    }

    /// Donates the `len` bytes at `start`, rounded inward to whole chunks.
    ///
    /// # Safety
    ///
    /// The span must be writable and stay so for as long as the heap lives, it
    /// must not overlap a span already donated, and nothing else may touch it
    /// while the heap owns it.
    pub unsafe fn extend(&mut self, start: *mut u8, len: usize) {
        let from = align_up(start.addr(), UNIT);
        let to = align_down(start.addr().saturating_add(len), UNIT);
        if to.saturating_sub(from) < MIN {
            return;
        }
        let block = start.with_addr(from).cast::<Free>();
        // SAFETY: the caller donated these bytes, and they are aligned and long
        // enough for the header written into them.
        unsafe {
            block.write(Free { size: to - from, next: null_mut() });
            self.size += to - from;
            self.insert(block);
        }
    }

    /// Carves `layout` out of the first chunk it fits in.
    pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let align = layout.align().max(UNIT);
        let need = align_up(layout.size().max(1), UNIT);
        let mut previous: *mut Free = null_mut();
        let mut chunk = self.free;

        while !chunk.is_null() {
            // SAFETY: the list holds only chunks this heap owns.
            let (size, next) = unsafe { ((*chunk).size, (*chunk).next) };
            let start = chunk.addr();
            let payload = align_up(start.saturating_add(UNIT), align);
            let finish = payload.saturating_add(need);
            if finish <= start + size {
                // SAFETY: the chunk is on the list, `previous` precedes it
                // there, and the payload was just proved to fit inside it.
                return Some(unsafe { self.carve(previous, chunk, payload, finish) });
            }
            previous = chunk;
            chunk = next;
        }
        None
    }

    /// Returns an allocation to the heap.
    ///
    /// # Safety
    ///
    /// `ptr` must come from [`allocate`](Self::allocate) on this heap and must
    /// not have been released since.
    pub unsafe fn deallocate(&mut self, ptr: NonNull<u8>) {
        // SAFETY: `allocate` wrote the header one unit below every payload it
        // returned, and `back` reaches the chunk that header describes.
        unsafe {
            let Live { size, back } = ptr.as_ptr().byte_sub(UNIT).cast::<Live>().read();
            let block = ptr.as_ptr().byte_sub(back & !TAG).cast::<Free>();
            block.write(Free { size, next: null_mut() });
            self.used -= size;
            self.insert(block);
        }
    }

    /// Bytes donated to the heap.
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Bytes held by live allocations, headers and rounding included.
    pub const fn used(&self) -> usize {
        self.used
    }

    /// Bytes on the free list, which one allocation can claim only if they are
    /// in one chunk.
    pub const fn free(&self) -> usize {
        self.size - self.used
    }

    /// Splits `chunk` at `finish`, unlinks what is taken, and stamps its
    /// header.
    ///
    /// # Safety
    ///
    /// `chunk` must be on the free list with `previous` before it, and
    /// `[payload, finish)` must lie inside it with room for a header below.
    unsafe fn carve(
        &mut self,
        previous: *mut Free,
        chunk: *mut Free,
        payload: usize,
        finish: usize,
    ) -> NonNull<u8> {
        // SAFETY: `chunk` and `previous` are list entries this heap owns, and
        // the split point lies inside `chunk` by contract.
        unsafe {
            let start = chunk.addr();
            let end = start + (*chunk).size;
            let next = (*chunk).next;
            let (taken, rest) = if end - finish >= MIN {
                let rest = chunk.with_addr(finish);
                rest.write(Free { size: end - finish, next });
                (finish - start, rest)
            } else {
                (end - start, next)
            };
            if previous.is_null() {
                self.free = rest;
            } else {
                (*previous).next = rest;
            }

            self.used += taken;
            let head = chunk.with_addr(payload - UNIT).cast::<Live>();
            debug_assert!((payload - start) & TAG == 0, "a payload with no room for its owner");
            head.write(Live { size: taken, back: (payload - start) | self.owner });
            NonNull::new_unchecked(chunk.with_addr(payload).cast::<u8>())
        }
    }

    /// Links `block` back in address order, merging it with either neighbour it
    /// touches.
    ///
    /// # Safety
    ///
    /// `block` must name a chunk of at least `MIN` bytes that this heap owns
    /// and that no chunk on the list overlaps.
    unsafe fn insert(&mut self, block: *mut Free) {
        // SAFETY: the list holds only chunks this heap owns, and `block` is one
        // of them by contract.
        unsafe {
            let mut previous: *mut Free = null_mut();
            let mut next = self.free;
            while !next.is_null() && next.addr() < block.addr() {
                previous = next;
                next = (*next).next;
            }

            (*block).next = next;
            if previous.is_null() {
                self.free = block;
            } else {
                (*previous).next = block;
            }
            if !next.is_null() && block.addr() + (*block).size == next.addr() {
                (*block).size += (*next).size;
                (*block).next = (*next).next;
            }
            if !previous.is_null() && previous.addr() + (*previous).size == block.addr() {
                (*previous).size += (*block).size;
                (*previous).next = (*block).next;
            }
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

/// Which heap `ptr` came out of, read from the allocation itself.
///
/// # Safety
///
/// As [`Heap::deallocate`]: `ptr` must be a live allocation some heap carved.
pub unsafe fn owner(ptr: NonNull<u8>) -> usize {
    // SAFETY: `carve` wrote the header one unit below every payload.
    unsafe { ptr.as_ptr().byte_sub(UNIT).cast::<Live>().read().back & TAG }
}

const fn align_up(value: usize, align: usize) -> usize {
    value.saturating_add(align - 1) & !(align - 1)
}

const fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use std::vec;
    use std::vec::Vec;

    use super::{Heap, UNIT};

    /// A heap over `bytes`, which the caller keeps alive for its lifetime.
    fn heap(bytes: &mut Vec<u8>) -> Heap {
        let mut heap = Heap::new();
        // SAFETY: the vector outlives the heap and nothing else touches it.
        unsafe { heap.extend(bytes.as_mut_ptr(), bytes.len()) };
        heap
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn allocation_is_aligned() {
        let mut bytes = vec![0u8; 4096];
        let mut heap = heap(&mut bytes);

        let wide = heap.allocate(layout(8, 256)).unwrap();

        assert_eq!(wide.addr().get() % 256, 0, "wide alignment ignored");
    }

    #[test]
    fn freed_chunk_is_reused() {
        let mut bytes = vec![0u8; 4096];
        let mut heap = heap(&mut bytes);
        let first = heap.allocate(layout(64, 8)).unwrap();

        // SAFETY: `first` came from this heap and is live.
        unsafe { heap.deallocate(first) };
        let second = heap.allocate(layout(64, 8)).unwrap();

        assert_eq!(first, second);
        assert_eq!(heap.used(), 64 + UNIT);
    }

    #[test]
    fn neighbours_coalesce() {
        let mut bytes = vec![0u8; 4096];
        let mut heap = heap(&mut bytes);
        let chunks: Vec<_> =
            (0..3).map(|_| heap.allocate(layout(1024, 8)).unwrap()).collect::<Vec<_>>();

        for chunk in chunks {
            // SAFETY: each chunk came from this heap and is live.
            unsafe { heap.deallocate(chunk) };
        }

        assert!(heap.allocate(layout(3072, 8)).is_some(), "released chunks stayed apart");
    }

    #[test]
    fn exhaustion_is_reported() {
        let mut bytes = vec![0u8; 1024];
        let mut heap = heap(&mut bytes);

        assert!(heap.allocate(layout(4096, 8)).is_none());
        assert_eq!(heap.used(), 0, "a refused request took memory");
    }

    #[test]
    fn allocations_are_disjoint() {
        let mut bytes = vec![0u8; 4096];
        let mut heap = heap(&mut bytes);

        let first = heap.allocate(layout(48, 8)).unwrap();
        let second = heap.allocate(layout(48, 8)).unwrap();

        assert!(second.addr().get() >= first.addr().get() + 48, "allocations overlap");
    }

    #[test]
    fn donated_bytes_are_accounted() {
        let mut bytes = vec![0u8; 4096];
        let mut heap = heap(&mut bytes);

        let live = heap.allocate(layout(100, 8)).unwrap();

        assert!(heap.size() >= 4096 - UNIT);
        assert_eq!(heap.free(), heap.size() - heap.used());
        // SAFETY: `live` came from this heap and is live.
        unsafe { heap.deallocate(live) };
        assert_eq!(heap.used(), 0, "release left bytes accounted for");
    }
}
