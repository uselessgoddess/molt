//! Bounds checks that survive a misprediction.
//!
//! A domain names an offset, the kernel compares it against the extent, and
//! refuses what does not fit. That settles the architectural answer and nothing
//! else: the processor is free to predict the compare the other way, run the
//! load it was about to be told not to run, and leave a line in the cache the
//! domain can time afterwards. The check was right and the bytes still left the
//! building — Spectre v1, and `docs/threat-model.md` calls it the real one.
//!
//! So the answer goes into the value and not only into the control flow.
//! [`below`] and [`upto`] give a comparison back as a [`Mask`] — all ones when
//! it held, zero when it did not — and [`Mask::apply`] hands back a value that
//! is zero in exactly the case the branch was there to prevent, so a
//! mis-speculated load reads the front of the extent instead of past its end.
//! Linux calls the shape `array_index_nospec`. It is arithmetic rather than a
//! fence, which is why both ports can have it: x86_64 has `lfence` to pay for
//! and RISC-V has no ratified speculation barrier to reach for at all.
//!
//! Two properties make it work, and they live here rather than at the call
//! sites, where they would have to be re-established every time:
//!
//! - The mask comes from the bits. `(value < limit) as u64` is a comparison the
//!   compiler may emit as a branch and the processor will then predict, which
//!   is the thing being defended against.
//! - The mask is opaque to the optimizer. On the path where the check passed,
//!   LLVM can prove the mask is all ones and delete the `&` that is the entire
//!   mitigation, so every mask leaves this module through [`black_box`]. That
//!   is also why nothing here is `const fn`, and why the wire parser that uses
//!   it is not either.
//!
//! [`black_box`]: core::hint::black_box

use core::hint::black_box;

/// A comparison as bits: all ones when it held, zero when it did not.
///
/// A `bool` is a thing to branch on. This is a thing to multiply by, and the
/// difference is the point — see the module docs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mask(u64);

impl Mask {
    /// Whether the comparison held.
    ///
    /// The architectural answer, and safe to branch on as long as whatever the
    /// branch carries has been through [`apply`] first.
    ///
    /// [`apply`]: Mask::apply
    #[inline(always)]
    pub fn passed(self) -> bool {
        self.0 != 0
    }

    /// `value` when the comparison held, and zero when it did not.
    #[inline(always)]
    pub fn apply(self, value: u64) -> u64 {
        value & self.0
    }
}

/// `value < limit`, for every pair of numbers a domain can name.
#[inline(always)]
pub fn below(value: u64, limit: u64) -> Mask {
    // The borrow out of `value - limit` is the unsigned comparison the hardware
    // itself computes (Hacker's Delight 2-12). Taken over the shorter trick of
    // shifting the sign bit of the difference, which is only right while both
    // operands stay under 2^63 — true of an aperture offset, not true of an
    // address, and the difference between them is not worth remembering at a
    // call site.
    let borrow = (!value & limit) | (!(value ^ limit) & value.wrapping_sub(limit));
    Mask(black_box(0u64.wrapping_sub(borrow >> 63)))
}

/// `value <= limit`, which is the shape a bounds check takes when the value is
/// the end of a buffer and the limit is a length.
#[inline(always)]
pub fn upto(value: u64, limit: u64) -> Mask {
    Mask(!below(limit, value).0)
}

/// `value`, or zero when it is not below `limit`: an index that is in range
/// whether or not the branch rejecting it has resolved yet.
#[inline(always)]
pub fn index(value: u64, limit: u64) -> u64 {
    below(value, limit).apply(value)
}

#[cfg(test)]
mod tests {
    use super::{below, index, upto};

    /// Both sides of every boundary a comparison written the easy way trips
    /// over: the 32-bit edge the wire format sits on, and the sign bit, where a
    /// signed shift stops meaning what it looks like it means.
    const EDGES: [u64; 14] = [
        0,
        1,
        2,
        0x7fff_ffff,
        0x8000_0000,
        0x8000_0001,
        u32::MAX as u64,
        1 << 32,
        (1 << 32) + 1,
        i64::MAX as u64,
        1 << 63,
        (1 << 63) + 1,
        u64::MAX - 1,
        u64::MAX,
    ];

    #[test]
    fn masks_agree_with_comparison() {
        for value in EDGES {
            for limit in EDGES {
                assert_eq!(below(value, limit).passed(), value < limit, "{value:#x} < {limit:#x}");
                assert_eq!(upto(value, limit).passed(), value <= limit, "{value:#x} <= {limit:#x}");
            }
        }
    }

    /// The same, over a range small enough to check every pair, because the
    /// interesting failures of a bit trick are off-by-one and not far away.
    #[test]
    fn masks_agree_over_every_small_pair() {
        for value in 0..64u64 {
            for limit in 0..64u64 {
                assert_eq!(below(value, limit).passed(), value < limit);
                assert_eq!(upto(value, limit).passed(), value <= limit);
            }
        }
    }

    /// Nothing in between: a mask that is neither all ones nor zero would leak
    /// the bits it kept.
    #[test]
    fn masks_are_all_ones_or_nothing() {
        for value in EDGES {
            for limit in EDGES {
                for mask in [below(value, limit), upto(value, limit)] {
                    let kept = mask.apply(u64::MAX);
                    assert!(kept == u64::MAX || kept == 0, "a partial mask {kept:#x}");
                    assert_eq!(mask.passed(), kept == u64::MAX);
                }
            }
        }
    }

    #[test]
    fn out_of_range_index_becomes_zero() {
        assert_eq!(index(7, 8), 7, "the last index of an extent was masked away");
        assert_eq!(index(8, 8), 0);
        assert_eq!(index(u64::MAX, 8), 0, "an index at the top of the address space survived");
    }
}
