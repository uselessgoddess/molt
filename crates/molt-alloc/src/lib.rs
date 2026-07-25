//! The kernel heap: spans of donated RAM handed out as sized allocations.
//!
//! Molt ran on fixed arrays until the metadata tree outgrew the kernel stack.
//! [`Heap`] is the smallest thing that fixes that: one address-ordered free
//! list, first fit, immediate coalescing, no per-size classes. [`Global`] wraps
//! it for `#[global_allocator]`.
//!
//! Frames come from `molt_arch::FrameAllocator`; this crate never touches the
//! frame table, so a heap is testable on the host over any span of bytes.

#![no_std]
#[cfg(test)]
extern crate std;

mod global;
mod heap;

pub use crate::global::Global;
pub use crate::heap::Heap;
