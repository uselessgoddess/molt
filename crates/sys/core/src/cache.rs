//! Cache layout for contended words.
//!
//! Padding halved contended wake time but added 8% to completion round trips on
//! the reference x86_64 VM, while growing `Executor<256>` from 256 bytes to
//! 32 KiB. Callers therefore choose compact or padded storage explicitly.

use core::borrow::Borrow;
use core::ops::{Deref, DerefMut};

mod private {
    pub trait Sealed {}
}

/// Chooses how neighbouring slots are laid out.
#[doc(hidden)]
pub trait CacheLayout: private::Sealed {
    type Slot<T>: Borrow<T>;
}

/// Packs neighbouring slots without extra alignment.
#[derive(Clone, Copy, Debug, Default)]
pub struct Compact;

impl private::Sealed for Compact {}

impl CacheLayout for Compact {
    type Slot<T> = T;
}

/// Places every slot on a target-appropriate cache-line boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct Padded;

impl private::Sealed for Padded {}

impl CacheLayout for Padded {
    type Slot<T> = CachePadded<T>;
}

/// Pads and aligns a value to a target-appropriate cache line.
// Match crossbeam-utils: some targets use or prefetch 128-byte lines.
#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64"
    ),
    repr(align(128))
)]
#[cfg_attr(
    any(
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "hexagon"
    ),
    repr(align(32))
)]
#[cfg_attr(target_arch = "m68k", repr(align(16)))]
#[cfg_attr(target_arch = "s390x", repr(align(256)))]
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64",
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "hexagon",
        target_arch = "m68k",
        target_arch = "s390x"
    )),
    repr(align(64))
)]
#[repr(C)]
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

impl<T> Borrow<T> for CachePadded<T> {
    fn borrow(&self) -> &T {
        &self.0
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
    fn padding_is_typed() {
        assert!(align_of::<CachePadded<u8>>() > align_of::<u8>());
    }
}
