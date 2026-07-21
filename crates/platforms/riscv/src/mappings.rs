//! Every range the boot address space declared, and the memory attributes that
//! belong to it.
//!
//! The log exists so [`paging::verify_image_protection`](crate::paging) can
//! hand a portable [`Audit`](molt_arch::audit::Audit) the exact set of ranges
//! the kernel claims to have mapped. It carries two more jobs that only look
//! incidental:
//!
//! - It is the record that refuses a second mapping of a window already mapped.
//!   [`Mmio`](molt_arch::Mmio) is a *unique* handle to its registers, so handing
//!   out two windows over one range would break that promise silently, at the
//!   first pair of concurrent register writes rather than here.
//! - It is where the cacheability of a leaf comes from. Sv39 without the
//!   Svpbmt extension has no memory-type bits in a page-table entry at all, so
//!   reading the live tables back can never recover "this leaf is uncached".
//!   The attribute belongs to the physical address — the platform's PMAs make
//!   an access to a device hole uncached and an access to RAM cacheable — and
//!   the only thing that knows which physical ranges are device holes is the
//!   set of windows [`Inventory::device`](molt_arch::memory::Inventory::device)
//!   already proved were not RAM. That set is this log.
//!
//! Everything here is arithmetic over an array, so it is host-testable and the
//! parts that would otherwise need a live page table are kept out of it.

use molt_arch::MappingError;
use molt_arch::audit::{Contents, MappedRange};
use molt_arch::memory::Cache;

/// The largest number of usable free-RAM ranges the audit stores inline.
///
/// The QEMU `virt` build exposes one contiguous span, and no plausible RISC-V
/// board grows this to double digits; growing it here is a one-line change.
pub const MAX_RAM_RANGES: usize = 8;

/// The largest number of device windows the kernel may map.
///
/// One ECAM window plus a handful of BARs is what Stage 2.2 asks for. A
/// board that wants more gets [`MappingError::Backend`] from [`MappingLog::push`]
/// rather than a mapping nobody declared.
pub const MAX_DEVICE_RANGES: usize = 8;

/// `.text`, `.rodata`, and the writable image span, plus RAM and devices.
const CAPACITY: usize = 3 + MAX_RAM_RANGES + MAX_DEVICE_RANGES;

/// Every mapped range the boot address space declares, in one array so
/// the image audit can hand them to a portable check.
pub struct MappingLog {
    ranges: [MappedRange; CAPACITY],
    len: usize,
}

impl MappingLog {
    pub const fn new() -> Self {
        const EMPTY: MappedRange = MappedRange::ram(0, 0);
        Self { ranges: [EMPTY; CAPACITY], len: 0 }
    }

    /// Records one declared range, or reports that the log is full.
    pub fn push(&mut self, range: MappedRange) -> Result<(), MappingError> {
        if self.len >= self.ranges.len() {
            return Err(MappingError::Backend);
        }
        self.ranges[self.len] = range;
        self.len += 1;
        Ok(())
    }

    pub fn as_slice(&self) -> &[MappedRange] {
        &self.ranges[..self.len]
    }

    /// Whether any declared range shares an address with `start..end`.
    ///
    /// Asked before a device window is mapped: overlapping an existing range
    /// means either a second [`Mmio`](molt_arch::Mmio) over registers someone
    /// already holds, or a window that reaches into the image, and neither is
    /// something the mapper should quietly do.
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        self.as_slice().iter().any(|range| start < range.end() && range.start() < end)
    }

    /// The memory type an access to `address` actually gets.
    ///
    /// Not read out of the page-table entry, because Sv39 has no bits to read
    /// it from: it is a property of the physical address, and the declared
    /// device windows are the kernel's record of which physical addresses are
    /// device holes rather than RAM.
    pub fn cache(&self, address: u64) -> Cache {
        let device = self.as_slice().iter().any(|range| {
            range.contents() == Contents::Device
                && range.start() <= address
                && address < range.end()
        });
        if device { Cache::Device } else { Cache::WriteBack }
    }
}

impl Default for MappingLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use molt_arch::MappingError;
    use molt_arch::audit::MappedRange;
    use molt_arch::memory::Cache;

    use super::{CAPACITY, MappingLog};

    #[test]
    fn a_window_already_in_the_log_is_refused() {
        let mut log = MappingLog::new();
        log.push(MappedRange::device(0x3000_0000, 0x3001_0000)).expect("room for one window");

        assert!(log.overlaps(0x3000_0000, 0x3001_0000), "the identical window");
        assert!(log.overlaps(0x3000_f000, 0x3002_0000), "a window sharing its last page");
        assert!(log.overlaps(0x2fff_f000, 0x3000_1000), "a window sharing its first page");
    }

    #[test]
    fn a_window_beside_a_declared_range_is_allowed() {
        let mut log = MappingLog::new();
        log.push(MappedRange::device(0x3000_0000, 0x3001_0000)).expect("room for one window");

        assert!(!log.overlaps(0x3001_0000, 0x3002_0000), "the range is half-open");
        assert!(!log.overlaps(0x2fff_0000, 0x3000_0000), "and so is the query");
    }

    #[test]
    fn a_full_log_refuses_another_range() {
        let mut log = MappingLog::new();
        for index in 0..CAPACITY as u64 {
            let start = 0x1_0000 * (index + 1);
            log.push(MappedRange::ram(start, start + 0x1000)).expect("a range that fits");
        }

        let overflow = log.push(MappedRange::ram(0, 0x1000));

        assert_eq!(overflow, Err(MappingError::Backend), "a range nobody declares is worse");
    }

    #[test]
    fn only_a_declared_device_window_reads_back_as_uncached() {
        let mut log = MappingLog::new();
        log.push(MappedRange::ram(0x8000_0000, 0x8800_0000)).expect("room for RAM");
        log.push(MappedRange::device(0x3000_0000, 0x3001_0000)).expect("room for a window");

        assert_eq!(log.cache(0x3000_0000), Cache::Device);
        assert_eq!(log.cache(0x8000_0000), Cache::WriteBack);
        assert_eq!(log.cache(0x3001_0000), Cache::WriteBack, "one past the window");
    }
}
