//! Bounds checks that survive a misprediction.
//!
//! Refusing a bad offset architecturally is not enough: the processor may
//! predict the compare the other way, run the load, and leave a cache line the
//! domain can time (Spectre v1). So the answer goes into the value, not only
//! into the branch:
//!
//! ```
//! use molt_abi::nospec::Mask;
//!
//! let (offset, len, bytes) = (16u64, 8u64, 4096u64);
//! let inside = Mask::of(offset + len <= bytes);
//!
//! assert_eq!(inside.apply(offset), 16); // zero, had it not fit
//! assert!(inside.passed()); // safe to branch on, now that it has
//! ```
//!
//! Linux calls the shape `array_index_nospec`. Being arithmetic rather than a
//! fence is what lets RISC-V, which has no ratified speculation barrier, have
//! it too.

use core::arch::asm;

/// A comparison as bits: all ones when it held, zero when it did not.
///
/// A `bool` is something to branch on; this is something to mask with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mask(u64);

impl Mask {
    /// A comparison that `held`, widened to every bit and put beyond the
    /// optimizer's reach.
    ///
    /// The barrier is the mitigation: on the passing path the mask is provably
    /// all ones, so anything that sees through it deletes the `&` in
    /// [`apply`](Mask::apply). `black_box` compiles to this same empty `asm!`
    /// and is documented to promise nothing, which is not enough to rest on.
    /// Nothing that goes through here can be `const fn`.
    #[inline(always)]
    pub fn of(held: bool) -> Self {
        let mut mask = 0u64.wrapping_sub(held as u64);
        // SAFETY: an empty block reading and writing one register, touching no
        // memory and leaving the flags where it found them.
        unsafe {
            asm!("/* {mask} */", mask = inout(reg) mask, options(pure, nomem, nostack, preserves_flags))
        };
        Self(mask)
    }

    /// Whether the comparison held.
    ///
    /// Safe to branch on as long as whatever the branch carries has been
    /// through [`apply`](Mask::apply) first.
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

#[cfg(test)]
mod tests {
    use super::Mask;

    #[test]
    fn keeps_all_value_or_none() {
        for value in [0, 7, u32::MAX as u64, 1 << 63, u64::MAX] {
            assert_eq!(Mask::of(true).apply(value), value);
            assert_eq!(Mask::of(false).apply(value), 0);
        }
    }

    #[test]
    fn which_way_it_went() {
        assert!(Mask::of(true).passed());
        assert!(!Mask::of(false).passed());
    }
}
