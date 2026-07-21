//! Just enough ACPI to learn where configuration space is mapped.
//!
//! Configuration space is the one window the kernel cannot guess. The local
//! APIC has an architectural base and the COM1 registers are a convention as
//! old as the platform, but ECAM is wherever the firmware decided to put it,
//! and the only thing that says where is the MCFG table. A kernel that hardcodes
//! the address its emulator happens to use is a kernel that enumerates whatever
//! frame lives there on real hardware.
//!
//! Everything below the [`Physical`] trait is ordinary safe code over byte
//! slices, so the parser is exercised on the host against hand-built tables
//! rather than only on the machine it has to be right on. The unsafe part is
//! one implementation of that trait, and it is used only while the loader's
//! direct map is still live — reading firmware tables afterwards is impossible
//! by construction, since the kernel's own map covers usable RAM only.

use core::sync::atomic::{AtomicU64, Ordering};

/// Longest table this parser will look at. Real root tables hold a few dozen
/// pointers; a length past this is a corrupt header, not a long table.
const LIMIT: u32 = 64 * 1024;

/// Bytes of the header every ACPI table starts with.
const HEADER: usize = 36;

/// Where the loader said the root pointer is, or zero for "it did not say".
static RSDP: AtomicU64 = AtomicU64::new(0);

/// Records the loader's root pointer for later use by [`configuration`].
pub fn remember(address: Option<u64>) {
    RSDP.store(address.unwrap_or(0), Ordering::Release);
}

/// One MCFG allocation: a memory-mapped configuration region and the buses it
/// answers for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    pub base: u64,
    pub group: u16,
    pub first: u8,
    pub last: u8,
}

impl Segment {
    /// Bytes the region occupies: one 1 MiB window per bus it covers.
    pub const fn span(self) -> u64 {
        (self.last as u64 - self.first as u64 + 1) << 20
    }
}

/// Physical memory, as something that can be read a few bytes at a time.
pub trait Physical {
    /// The `len` bytes at physical address `at`, or `None` where they are not
    /// readable as one run.
    fn bytes(&self, at: u64, len: usize) -> Option<&[u8]>;
}

/// Physical memory reached through a live direct map at `offset`.
pub struct Direct {
    offset: u64,
}

impl Direct {
    /// # Safety
    ///
    /// `offset` must be a live direct map of all physical memory for as long as
    /// this value exists. Firmware tables are not RAM the kernel owns, so this
    /// is only true of the loader's map, and only before `CR3` is rewritten.
    pub const unsafe fn new(offset: u64) -> Self {
        Self { offset }
    }
}

impl Physical for Direct {
    fn bytes(&self, at: u64, len: usize) -> Option<&[u8]> {
        let start = self.offset.checked_add(at)?;
        start.checked_add(len as u64)?;
        // SAFETY: the caller of `new` vouched for a live direct map covering
        // every physical address, and the range was just checked not to wrap.
        Some(unsafe { core::slice::from_raw_parts(start as *const u8, len) })
    }
}

/// The first ECAM allocation the firmware describes, if it describes any.
///
/// A machine with no MCFG has no memory-mapped configuration space, which is a
/// fact about the machine rather than an error: the caller skips the PCI path.
pub fn configuration<P: Physical>(memory: &P) -> Option<Segment> {
    mcfg(memory, RSDP.load(Ordering::Acquire))
}

/// Walks RSDP to root table to MCFG, checking every signature and checksum.
pub fn mcfg<P: Physical>(memory: &P, rsdp: u64) -> Option<Segment> {
    let table = find(memory, rsdp, b"MCFG")?;
    // Header, then eight reserved bytes, then 16-byte allocations.
    entry(table, 0).map(|allocation| Segment {
        base: read64(allocation, 0),
        group: read16(allocation, 8),
        first: allocation[10],
        last: allocation[11],
    })
}

/// The `index`th 16-byte allocation of an MCFG body.
fn entry(table: &[u8], index: usize) -> Option<&[u8]> {
    let start = HEADER + 8 + index * 16;
    table.get(start..start + 16)
}

/// Finds the table with `signature` through the root table the RSDP names.
fn find<'p, P: Physical>(memory: &'p P, rsdp: u64, signature: &[u8; 4]) -> Option<&'p [u8]> {
    if rsdp == 0 {
        return None;
    }
    let head = memory.bytes(rsdp, 20)?;
    if &head[..8] != b"RSD PTR " || !sums_to_zero(head) {
        return None;
    }

    // Revision two put a 64-bit root pointer at the end of a longer structure,
    // with its own checksum over the whole of it. The 32-bit one is still there
    // and still valid, but on a machine with tables above 4 GiB it cannot name
    // them, so the wider pointer is preferred wherever the firmware offers one.
    let (root, width) = match head[15] >= 2 {
        true => {
            let length = read32(memory.bytes(rsdp, 24)?, 20);
            let extended = memory.bytes(rsdp, length.min(LIMIT) as usize)?;
            match length >= 33 && sums_to_zero(extended) {
                true => (read64(extended, 24), 8),
                false => (u64::from(read32(head, 16)), 4),
            }
        }
        false => (u64::from(read32(head, 16)), 4),
    };

    let root = table(memory, root)?;
    let expected: &[u8; 4] = match width {
        8 => b"XSDT",
        _ => b"RSDT",
    };
    if &root[..4] != expected {
        return None;
    }
    for pointer in root[HEADER..].chunks_exact(width) {
        let at = match width {
            8 => read64(pointer, 0),
            _ => u64::from(read32(pointer, 0)),
        };
        let Some(candidate) = table(memory, at) else { continue };
        if &candidate[..4] == signature {
            return Some(candidate);
        }
    }
    None
}

/// One whole table, read at its own declared length and checksummed.
fn table<'p, P: Physical>(memory: &'p P, at: u64) -> Option<&'p [u8]> {
    let header = memory.bytes(at, HEADER)?;
    let length = read32(header, 4);
    if length < HEADER as u32 || length > LIMIT {
        return None;
    }
    let table = memory.bytes(at, length as usize)?;
    sums_to_zero(table).then_some(table)
}

/// Every ACPI structure carries a byte that makes its bytes sum to zero, which
/// is the only evidence available that the pointer led somewhere real.
fn sums_to_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn read16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn read64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec;
    use std::vec::Vec;

    use super::{Physical, Segment, mcfg};

    /// A flat block of physical memory starting at `base`.
    struct Memory {
        base: u64,
        bytes: Vec<u8>,
    }

    impl Memory {
        fn new(base: u64, len: usize) -> Self {
            Self { base, bytes: vec![0; len] }
        }

        fn put(&mut self, at: u64, bytes: &[u8]) {
            let start = (at - self.base) as usize;
            self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
        }

        /// Writes the checksum byte that makes `[at, at + len)` sum to zero.
        fn seal(&mut self, at: u64, len: usize, checksum: usize) {
            let start = (at - self.base) as usize;
            self.bytes[start + checksum] = 0;
            let sum =
                self.bytes[start..start + len].iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
            self.bytes[start + checksum] = sum.wrapping_neg();
        }
    }

    impl Physical for Memory {
        fn bytes(&self, at: u64, len: usize) -> Option<&[u8]> {
            let start = at.checked_sub(self.base)? as usize;
            self.bytes.get(start..start + len)
        }
    }

    /// A table header with `signature` and `length`, checksum left unsealed.
    fn header(signature: &[u8; 4], length: u32) -> Vec<u8> {
        let mut header = vec![0u8; 36];
        header[..4].copy_from_slice(signature);
        header[4..8].copy_from_slice(&length.to_le_bytes());
        header
    }

    /// An MCFG at 0x2000 describing one allocation, an XSDT at 0x1000 pointing
    /// at it, and a revision-two RSDP at 0x500 pointing at that.
    fn firmware() -> Memory {
        let mut memory = Memory::new(0x500, 0x4000);

        let mut mcfg = header(b"MCFG", 60);
        mcfg.extend_from_slice(&[0; 8]);
        mcfg.extend_from_slice(&0xb000_0000u64.to_le_bytes());
        mcfg.extend_from_slice(&0u16.to_le_bytes());
        mcfg.extend_from_slice(&[0, 0x7f]);
        mcfg.extend_from_slice(&[0; 4]);
        memory.put(0x2000, &mcfg);
        memory.seal(0x2000, 60, 9);

        let mut xsdt = header(b"XSDT", 44);
        xsdt.extend_from_slice(&0x2000u64.to_le_bytes());
        memory.put(0x1000, &xsdt);
        memory.seal(0x1000, 44, 9);

        let mut rsdp = vec![0u8; 36];
        rsdp[..8].copy_from_slice(b"RSD PTR ");
        rsdp[15] = 2;
        rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
        rsdp[24..32].copy_from_slice(&0x1000u64.to_le_bytes());
        memory.put(0x500, &rsdp);
        memory.seal(0x500, 20, 8);
        memory.seal(0x500, 36, 32);

        memory
    }

    #[test]
    fn an_allocation_says_which_buses_it_answers_for() {
        let found = mcfg(&firmware(), 0x500);

        assert_eq!(found, Some(Segment { base: 0xb000_0000, group: 0, first: 0, last: 0x7f }));
        assert_eq!(found.unwrap().span(), 0x800_0000);
    }

    #[test]
    fn a_corrupt_root_pointer_describes_nothing() {
        let mut memory = firmware();
        memory.put(0x500 + 8, &[0xff]);

        assert_eq!(mcfg(&memory, 0x500), None, "a bad checksum was believed");
    }

    #[test]
    fn a_corrupt_table_describes_nothing() {
        let mut memory = firmware();
        memory.put(0x2000 + 40, &[0xff]);

        assert_eq!(mcfg(&memory, 0x500), None, "a bad checksum was believed");
    }

    #[test]
    fn a_machine_without_the_table_says_so() {
        let mut memory = firmware();
        memory.put(0x2000, b"HPET");
        memory.seal(0x2000, 60, 9);

        assert_eq!(mcfg(&memory, 0x500), None);
    }

    #[test]
    fn a_loader_that_found_no_tables_says_so() {
        assert_eq!(mcfg(&firmware(), 0), None);
    }

    #[test]
    fn a_revision_one_pointer_is_followed_through_the_rsdt() {
        let mut memory = firmware();
        let mut rsdt = header(b"RSDT", 40);
        rsdt.extend_from_slice(&0x2000u32.to_le_bytes());
        memory.put(0x3000, &rsdt);
        memory.seal(0x3000, 40, 9);
        memory.put(0x500 + 15, &[1]);
        memory.put(0x500 + 16, &0x3000u32.to_le_bytes());
        memory.seal(0x500, 20, 8);

        assert_eq!(mcfg(&memory, 0x500).map(|segment| segment.base), Some(0xb000_0000));
    }
}
