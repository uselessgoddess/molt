//! Optional cache-line padding for contended words.
//!
//! Two threads writing neighbouring atomics in the same cache line bounce that
//! line between their caches even though the writes are independent. Padding
//! each word onto its own line removes the bounce — and costs memory the kernel
//! cannot get back, because Molt has no allocator and these arrays are `static`.
//!
//! Which side wins depends on the deployment, not on the code, so it is a
//! feature rather than a decision baked into the type. Off by default:
//!
//! - `Executor<256>` is 256 bytes unpadded and 32 KiB padded.
//! - `CompletionSlab<u64, 256>` grows by roughly the same factor.
//!
//! On a single-core or lightly contended kernel that is 32 KiB spent to avoid
//! contention that never happens. Turn `cache-padded` on when several harts
//! wake tasks or complete requests concurrently, and measure:
//!
//! ```text
//! cargo bench -p molt-core --bench scheduler -- --save-baseline unpadded
//! cargo bench -p molt-core --bench scheduler --features cache-padded -- --baseline unpadded
//! ```
//!
//! On a 4-core x86_64 Linux VM that reports roughly −50% on
//! `executor_contended_wake` and roughly +8% on `completion_round_trip`, which
//! is the trade in one line: padding buys cross-hart wakes and charges the
//! single-threaded scan, whose slots no longer share a line. Both numbers move
//! with the machine, so treat them as a shape rather than a target.
//!
//! The alignment is 128 bytes, not 64. x86_64 prefetches cache lines in pairs
//! and Apple's aarch64 cores use 128-byte lines, so 64 leaves false sharing on
//! the table on exactly the machines this is meant to help.

use core::ops::{Deref, DerefMut};

/// Wraps a value so it does not share a cache line with its neighbours.
///
/// Without the `cache-padded` feature this is a transparent newtype and adds
/// nothing to the layout.
#[cfg_attr(feature = "cache-padded", repr(align(128)))]
#[derive(Debug, Default)]
pub struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for CachePadded<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::mem::align_of;

    use super::CachePadded;

    #[test]
    fn padding_follows_the_feature() {
        let padded = align_of::<CachePadded<u8>>() > align_of::<u8>();
        assert_eq!(padded, cfg!(feature = "cache-padded"));
    }
}
