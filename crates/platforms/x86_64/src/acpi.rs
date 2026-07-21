//! Where firmware hid the ECAM window, read out of the ACPI tables.
//!
//! Every field decision here comes from a byte offset in the ACPI spec, and
//! every table arrives as untrusted firmware data, so the parsing is split in
//! two. The pure half takes `&[u8]` and never indexes without a bound, which
//! makes it testable on the host with hand-built images; the only `unsafe` is
//! the reader that turns a physical address into one of those slices via the
//! direct map. Table entries are read byte-wise because firmware aligns the
//! XSDT to 4 bytes, so a `*const u64` over its entries would be misaligned.

use molt_arch::ConfigSpace;

/// Errors the tables can hand us, all of them a refusal to trust firmware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiError {
    /// The bytes at the reported address are not an RSDP.
    Rsdp,
    /// A table's bytes do not sum to zero.
    Checksum,
    /// The RSDP predates ACPI 2.0, so there is no XSDT to walk.
    Revision,
    /// A table carries a different signature than the one asked for.
    Signature,
    /// A table's `length` is implausible or reaches past the bytes we have.
    Truncated,
    /// No MCFG table, or an MCFG with no allocation in it.
    Missing,
    /// The allocation's bus range is one `ConfigSpace` refuses.
    Range,
}

/// Bytes of the header every ACPI table starts with.
const HEADER: usize = 36;

/// Nothing we parse is anywhere near this large; a bigger `length` is garbage.
const MAX_TABLE: usize = 64 * 1024;

/// The XSDT's physical address, from the RSDP image firmware pointed at.
pub fn rsdp(bytes: &[u8]) -> Result<u64, AcpiError> {
    let first = bytes.get(..20).ok_or(AcpiError::Truncated)?;
    if &first[..8] != b"RSD PTR " {
        return Err(AcpiError::Rsdp);
    }
    if sum(first) != 0 {
        return Err(AcpiError::Checksum);
    }
    // Revision 0 is ACPI 1.0: the RSDP stops at 20 bytes and has no XSDT.
    if first[15] < 2 {
        return Err(AcpiError::Revision);
    }

    // The revision 2 RSDP is 36 bytes; anything shorter cannot hold an XSDT
    // address and its extended checksum.
    let length = le(&bytes[20..24]) as usize;
    if length < HEADER {
        return Err(AcpiError::Truncated);
    }
    let extended = bytes.get(..length).ok_or(AcpiError::Truncated)?;
    if sum(extended) != 0 {
        return Err(AcpiError::Checksum);
    }

    Ok(le(&bytes[24..32]))
}

/// The physical addresses of the tables an XSDT image lists.
pub fn xsdt_entries(bytes: &[u8]) -> Result<impl Iterator<Item = u64> + '_, AcpiError> {
    let body = table(bytes, b"XSDT")?;
    Ok(body[HEADER..].chunks_exact(8).map(le))
}

/// The configuration space of the first allocation in an MCFG image.
pub fn mcfg(bytes: &[u8]) -> Result<ConfigSpace, AcpiError> {
    let body = table(bytes, b"MCFG")?;
    // 8 reserved bytes follow the header before the first 16-byte allocation.
    let allocation = body.get(44..60).ok_or(AcpiError::Missing)?;

    let base = le(&allocation[..8]);
    let segment = allocation[8] as u16 | (allocation[9] as u16) << 8;
    ConfigSpace::new(base, segment, allocation[10], allocation[11]).map_err(|_| AcpiError::Range)
}

/// Finds the PCI configuration space described by ACPI.
///
/// # Safety
/// `rsdp_physical` must be the address firmware reported, and `offset` must be
/// a complete direct map of physical memory that stays valid for the call.
pub unsafe fn config_space(rsdp_physical: u64, offset: u64) -> Result<ConfigSpace, AcpiError> {
    // SAFETY: the caller promises the direct map covers the RSDP, whose
    // extended form is 36 bytes.
    let root = unsafe { image(offset, rsdp_physical, HEADER) };
    let xsdt_physical = rsdp(root)?;

    // SAFETY: as above; the header is what tells us how much more to read.
    let header = unsafe { image(offset, xsdt_physical, HEADER) };
    let xsdt_length = length(header)?;
    // SAFETY: as above, now for the length the header just vouched for.
    let xsdt = unsafe { image(offset, xsdt_physical, xsdt_length) };

    for entry in xsdt_entries(xsdt)? {
        // SAFETY: as above; a listed table starts with its 4-byte signature.
        if unsafe { image(offset, entry, 4) } != b"MCFG" {
            continue;
        }
        // SAFETY: as above, for the header and then the whole table.
        let header = unsafe { image(offset, entry, HEADER) };
        let mcfg_length = length(header)?;
        // SAFETY: as above.
        return mcfg(unsafe { image(offset, entry, mcfg_length) });
    }

    Err(AcpiError::Missing)
}

/// A table's checksummed body, once its signature and length are believable.
fn table<'bytes>(bytes: &'bytes [u8], signature: &[u8; 4]) -> Result<&'bytes [u8], AcpiError> {
    let length = length(bytes)?;
    if &bytes[..4] != signature {
        return Err(AcpiError::Signature);
    }

    let body = bytes.get(..length).ok_or(AcpiError::Truncated)?;
    if sum(body) != 0 {
        return Err(AcpiError::Checksum);
    }

    Ok(body)
}

/// The `length` field of a table header, refused when it cannot be one.
fn length(bytes: &[u8]) -> Result<usize, AcpiError> {
    let header = bytes.get(..HEADER).ok_or(AcpiError::Truncated)?;
    let length = le(&header[4..8]) as usize;
    if !(HEADER..=MAX_TABLE).contains(&length) {
        return Err(AcpiError::Truncated);
    }
    Ok(length)
}

/// Reads up to 8 little-endian bytes without assuming the slice is aligned.
fn le(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |value, &byte| value << 8 | byte as u64)
}

fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte))
}

/// Borrows `len` bytes of physical memory through the direct map.
///
/// # Safety
/// `offset` must map all of physical memory, and `physical..physical + len`
/// must stay valid and unwritten for the returned lifetime.
unsafe fn image<'map>(offset: u64, physical: u64, len: usize) -> &'map [u8] {
    // SAFETY: the caller guarantees the mapping; the sum cannot wrap for a
    // real direct map, and wrapping here only produces an address the caller
    // already vouched for as unreachable.
    unsafe { core::slice::from_raw_parts(offset.wrapping_add(physical) as *const u8, len) }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::{AcpiError, mcfg, rsdp, xsdt_entries};

    /// Writes the byte that makes `covered` bytes sum to zero.
    fn checksum(bytes: &mut [u8], index: usize, covered: usize) {
        bytes[index] = 0;
        let sum = bytes[..covered].iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte));
        bytes[index] = sum.wrapping_neg();
    }

    fn rsdp_image(xsdt: u64, revision: u8) -> [u8; 36] {
        let mut bytes = [0u8; 36];
        bytes[..8].copy_from_slice(b"RSD PTR ");
        bytes[15] = revision;
        bytes[20..24].copy_from_slice(&36u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&xsdt.to_le_bytes());
        checksum(&mut bytes, 8, 20);
        checksum(&mut bytes, 32, 36);
        bytes
    }

    fn xsdt_image(entries: [u64; 2]) -> [u8; 52] {
        let mut bytes = [0u8; 52];
        bytes[..4].copy_from_slice(b"XSDT");
        bytes[4..8].copy_from_slice(&52u32.to_le_bytes());
        bytes[36..44].copy_from_slice(&entries[0].to_le_bytes());
        bytes[44..52].copy_from_slice(&entries[1].to_le_bytes());
        checksum(&mut bytes, 9, 52);
        bytes
    }

    fn mcfg_image(base: u64, segment: u16, first_bus: u8, last_bus: u8) -> [u8; 60] {
        let mut bytes = [0u8; 60];
        bytes[..4].copy_from_slice(b"MCFG");
        bytes[4..8].copy_from_slice(&60u32.to_le_bytes());
        bytes[44..52].copy_from_slice(&base.to_le_bytes());
        bytes[52..54].copy_from_slice(&segment.to_le_bytes());
        bytes[54] = first_bus;
        bytes[55] = last_bus;
        checksum(&mut bytes, 9, 60);
        bytes
    }

    #[test]
    fn rsdp_reports_the_xsdt_address() {
        let bytes = rsdp_image(0x7fff_0000, 2);

        let xsdt = rsdp(&bytes);

        assert_eq!(xsdt, Ok(0x7fff_0000), "the extended pointer firmware wrote");
    }

    #[test]
    fn bad_checksum_is_refused() {
        let mut bytes = rsdp_image(0x7fff_0000, 2);

        bytes[24] ^= 0xff;

        assert_eq!(rsdp(&bytes), Err(AcpiError::Checksum));
    }

    #[test]
    fn acpi_one_rsdp_is_refused() {
        let bytes = rsdp_image(0, 0);

        let xsdt = rsdp(&bytes);

        assert_eq!(xsdt, Err(AcpiError::Revision), "revision 0 has no XSDT to point at");
    }

    #[test]
    fn truncated_mcfg_is_refused() {
        let bytes = mcfg_image(0xb000_0000, 0, 0, 0xff);

        let space = mcfg(&bytes[..50]);

        assert_eq!(space, Err(AcpiError::Truncated), "the length reaches past the bytes");
    }

    #[test]
    fn mcfg_reports_the_ecam_window() {
        let bytes = mcfg_image(0xb000_0000, 1, 0x10, 0x20);

        let space = mcfg(&bytes).expect("a well-formed allocation");

        assert_eq!(space.span().expect("an ECAM span").start(), 0xb000_0000);
        assert_eq!((space.segment(), space.first_bus(), space.last_bus()), (1, 0x10, 0x20));
    }

    #[test]
    fn unaligned_xsdt_entries_are_read() {
        let image = xsdt_image([0x7fff_1000, 0x7fff_2000]);
        let mut buffer = [0u8; 55];
        buffer[3..].copy_from_slice(&image);

        let entries: Vec<u64> = xsdt_entries(&buffer[3..]).expect("a well-formed XSDT").collect();

        assert_eq!(entries, [0x7fff_1000, 0x7fff_2000], "entries at odd addresses");
    }
}
