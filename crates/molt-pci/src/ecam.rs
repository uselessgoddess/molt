//! Configuration space reached the memory-mapped way.

use crate::address::{Address, WINDOW};
use crate::config::Config;

/// Bytes of configuration space one bus occupies.
pub const BUS: u64 = 1 << 20;

/// A mapped enhanced configuration access region.
///
/// The pointer is the whole of the unsafety, and it is taken once: every read
/// below is bounds-checked against the bus range the mapping covers, so a
/// walker that wanders off the end of a bridge reads all ones — absence — the
/// same as it would on the bus.
pub struct Ecam {
    base: *mut u32,
    first: u8,
    last: u8,
}

impl Ecam {
    /// # Safety
    ///
    /// `base` must be a live mapping of the segment's configuration region for
    /// buses `first..=last`, mapped as device memory, and it must stay mapped
    /// for as long as this value exists.
    pub const unsafe fn new(base: *mut u32, first: u8, last: u8) -> Self {
        Self { base, first, last }
    }

    /// Bytes a bus range occupies, which is what the caller has to map before
    /// it can call [`new`](Ecam::new).
    pub const fn span(first: u8, last: u8) -> u64 {
        (last as u64 - first as u64 + 1) * BUS
    }

    fn word(&self, at: Address, offset: u16) -> Option<*mut u32> {
        if at.bus() < self.first || at.bus() > self.last {
            return None;
        }
        if offset & 3 != 0 || usize::from(offset) >= WINDOW {
            return None;
        }
        let byte = at.window() - (self.first as usize) * BUS as usize + usize::from(offset);
        // SAFETY: `byte` is below the span the caller of `new` mapped, since
        // the bus is inside the range and the offset inside one window.
        Some(unsafe { self.base.byte_add(byte) })
    }
}

impl Config for Ecam {
    fn read(&self, at: Address, offset: u16) -> u32 {
        match self.word(at, offset) {
            // SAFETY: `word` returns a pointer inside the mapping, and
            // configuration space tolerates any aligned 32-bit read.
            Some(word) => unsafe { word.read_volatile() },
            None => !0,
        }
    }

    fn write(&self, at: Address, offset: u16, value: u32) {
        if let Some(word) = self.word(at, offset) {
            // SAFETY: as in `read`; the write is aligned and inside the window.
            unsafe { word.write_volatile(value) };
        }
    }

    fn buses(&self) -> (u8, u8) {
        (self.first, self.last)
    }
}
