//! Where firmware hid the ECAM window, read out of the flattened device tree.
//!
//! This is the RISC-V half of the question `acpi.rs` answers on x86_64, and it
//! is split the same way: a pure reader over `&[u8]` that never indexes without
//! a bound, plus one `unsafe fn` that turns the pointer OpenSBI leaves in `a1`
//! into such a slice. Everything here is host-testable because the blob is just
//! bytes.
//!
//! Only the ECAM window is decoded. A general device tree API would be a larger
//! and more speculative thing; the kernel currently needs one fact from
//! firmware, so the walk answers that one question and forgets the tree. Two
//! bounds — a token cap and a depth cap — mean a corrupt blob ends the walk
//! with an error instead of spinning the boot hart forever.

use molt_arch::ConfigSpace;
use molt_arch::pci::BUS_STRIDE;

/// Everything a blob can be wrong about, all of them a refusal to trust it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FdtError {
    /// The bytes do not start with the flattened device tree magic.
    Magic,
    /// The blob's structure is newer than version 16, which we cannot read.
    Version,
    /// A field, token, or string reaches past the bytes we have.
    Truncated,
    /// No `pci-host-ecam-generic` node in the tree.
    Missing,
    /// A token, cell count, or property length that no conforming blob emits.
    Malformed,
    /// The window firmware reported cannot describe the bus range it claims.
    Range,
}

/// A borrowed, validated flattened device tree.
pub struct DeviceTree<'dtb> {
    bytes: &'dtb [u8],
    size: usize,
}

/// The `compatible` string of the ECAM host bridge the kernel drives.
const ECAM: &[u8] = b"pci-host-ecam-generic";

/// Bytes of the header, through `size_dt_struct`.
const HEADER: usize = 40;

const MAGIC: u32 = 0xd00d_feed;

/// The structure version this reader implements; a blob whose
/// `last_comp_version` is higher is one we would misread.
const COMPATIBLE_VERSION: u32 = 16;

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

/// A walk longer than this is a corrupt blob, not a large machine: QEMU `virt`
/// with 32 virtio slots emits a few thousand tokens.
const MAX_TOKENS: usize = 1 << 16;

/// Nesting deeper than this is likewise garbage; real trees reach four or five.
const MAX_DEPTH: usize = 32;

impl<'dtb> DeviceTree<'dtb> {
    /// Validates the header and borrows the whole blob.
    pub fn new(bytes: &'dtb [u8]) -> Result<Self, FdtError> {
        let size = size(bytes)?;
        // Everything after this point may trust `size` bytes to be present.
        if bytes.len() < size {
            return Err(FdtError::Truncated);
        }
        Ok(Self { bytes, size })
    }

    /// The total size the header reports, so a caller can re-borrow exactly.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The ECAM window of the first `pci-host-ecam-generic` node.
    pub fn config_space(&self) -> Result<ConfigSpace, FdtError> {
        let structs = self.block(8, 36)?;
        let strings = self.block(12, 32)?;

        let mut cursor = 0;
        // Cells declared *by* the node open at each depth, for its children.
        let mut cells = [Cells::DEFAULT; MAX_DEPTH];
        let mut depth = 0;
        let mut node = Node::new(Cells::DEFAULT);

        for _ in 0..MAX_TOKENS {
            let token = be32(structs, cursor)?;
            cursor += 4;
            match token {
                BEGIN_NODE => {
                    // Properties precede subnodes, so an open node is complete
                    // the moment a child opens.
                    if node.matched {
                        return node.config_space();
                    }
                    cursor = skip_name(structs, cursor)?;
                    if depth >= MAX_DEPTH {
                        return Err(FdtError::Malformed);
                    }
                    let parent = if depth == 0 { Cells::DEFAULT } else { cells[depth - 1] };
                    cells[depth] = Cells::DEFAULT;
                    node = Node::new(parent);
                    depth += 1;
                }
                PROP => {
                    let len = be32(structs, cursor)? as usize;
                    let nameoff = be32(structs, cursor + 4)? as usize;
                    let start = cursor + 8;
                    let end = start.checked_add(len).ok_or(FdtError::Truncated)?;
                    let value = structs.get(start..end).ok_or(FdtError::Truncated)?;
                    cursor = align(end)?;
                    if depth == 0 {
                        return Err(FdtError::Malformed);
                    }
                    node.property(name(strings, nameoff)?, value, &mut cells[depth - 1]);
                }
                END_NODE => {
                    if node.matched {
                        return node.config_space();
                    }
                    depth = depth.checked_sub(1).ok_or(FdtError::Malformed)?;
                    // The parent reopens with its properties already read, so
                    // nothing it declares can still arrive.
                    node = Node::new(Cells::DEFAULT);
                }
                NOP => {}
                END => return Err(FdtError::Missing),
                _ => return Err(FdtError::Malformed),
            }
        }

        Err(FdtError::Malformed)
    }

    /// One of the two blocks the header locates by offset and size.
    fn block(&self, offset_at: usize, size_at: usize) -> Result<&'dtb [u8], FdtError> {
        let offset = be32(self.bytes, offset_at)? as usize;
        let len = be32(self.bytes, size_at)? as usize;
        let end = offset.checked_add(len).ok_or(FdtError::Truncated)?;
        if end > self.size {
            return Err(FdtError::Truncated);
        }
        self.bytes.get(offset..end).ok_or(FdtError::Truncated)
    }
}

/// Borrows a device tree firmware left in memory.
///
/// # Safety
/// `address` must be the device tree pointer firmware passed, mapped and
/// immutable for `'dtb`.
pub unsafe fn at<'dtb>(address: usize) -> Result<DeviceTree<'dtb>, FdtError> {
    // The header alone says how much more there is, so it is read first and the
    // blob re-borrowed at exactly the size it vouches for.
    // SAFETY: the caller promises a mapped device tree, which is at least a
    // header long.
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, HEADER) };
    let size = size(header)?;

    // SAFETY: as above, now for the size the header just vouched for.
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
    DeviceTree::new(bytes)
}

/// The ECAM window of the device tree firmware left at `address`.
///
/// A null address is the one case that cannot be answered by reading: a hart
/// entered without a device tree pointer arrives here with zero in `a1`, and
/// dereferencing it is a fault rather than a diagnosis. It is refused as
/// [`FdtError::Missing`] before any load, which is also what makes calling this
/// with zero safe.
///
/// # Safety
/// `address` must be zero, or the device tree pointer firmware passed, mapped
/// and immutable for the duration of the call.
pub unsafe fn config_space_at(address: usize) -> Result<ConfigSpace, FdtError> {
    if address == 0 {
        return Err(FdtError::Missing);
    }
    // SAFETY: the caller promises a mapped device tree at a non-zero address,
    // and the tree is only borrowed for this call.
    let tree = unsafe { at(address)? };
    tree.config_space()
}

/// The `totalsize` of a blob whose magic and version we accept.
fn size(bytes: &[u8]) -> Result<usize, FdtError> {
    if be32(bytes, 0)? != MAGIC {
        return Err(FdtError::Magic);
    }
    if be32(bytes, 24)? > COMPATIBLE_VERSION {
        return Err(FdtError::Version);
    }
    let size = be32(bytes, 4)? as usize;
    if size < HEADER {
        return Err(FdtError::Truncated);
    }
    Ok(size)
}

/// The cell widths a node imposes on its children's `reg` properties.
#[derive(Clone, Copy)]
struct Cells {
    address: u32,
    size: u32,
}

impl Cells {
    /// What QEMU `virt` declares at the root, and what we assume when a parent
    /// declares nothing: an address and a size that are both 64 bits.
    const DEFAULT: Self = Self { address: 2, size: 2 };
}

/// The properties of the node currently open, kept only until it closes.
struct Node<'dtb> {
    matched: bool,
    cells: Cells,
    reg: Option<&'dtb [u8]>,
    bus_range: Option<&'dtb [u8]>,
    domain: Option<&'dtb [u8]>,
}

impl<'dtb> Node<'dtb> {
    fn new(cells: Cells) -> Self {
        Self { matched: false, cells, reg: None, bus_range: None, domain: None }
    }

    /// Records one property, and any cell width it declares for its children.
    fn property(&mut self, name: &[u8], value: &'dtb [u8], children: &mut Cells) {
        match name {
            // `compatible` is a list of NUL-separated strings, most specific
            // first, and the generic ECAM binding is rarely the first of them.
            b"compatible" => self.matched |= value.split(|&byte| byte == 0).any(|it| it == ECAM),
            b"reg" => self.reg = Some(value),
            b"bus-range" => self.bus_range = Some(value),
            b"linux,pci-domain" => self.domain = Some(value),
            b"#address-cells" => children.address = cell(value).unwrap_or(children.address),
            b"#size-cells" => children.size = cell(value).unwrap_or(children.size),
            _ => {}
        }
    }

    fn config_space(&self) -> Result<ConfigSpace, FdtError> {
        let (address, size) = (self.cells.address, self.cells.size);
        // One cell is 32 bits and two are 64; anything else is a binding this
        // reader would silently misdecode.
        if !(1..=2).contains(&address) || !(1..=2).contains(&size) {
            return Err(FdtError::Malformed);
        }

        let reg = self.reg.ok_or(FdtError::Malformed)?;
        let split = address as usize * 4;
        let base = cells(reg.get(..split).ok_or(FdtError::Malformed)?);
        let window = cells(reg.get(split..split + size as usize * 4).ok_or(FdtError::Malformed)?);

        let (first_bus, last_bus) = self.buses()?;
        // A window shorter than the buses it claims would let a later config
        // read walk off the end of the mapping.
        let claimed = (u64::from(last_bus) - u64::from(first_bus) + 1) * BUS_STRIDE;
        if window < claimed {
            return Err(FdtError::Range);
        }

        ConfigSpace::new(base, self.segment()?, first_bus, last_bus).map_err(|_| FdtError::Range)
    }

    /// The bus range, defaulting to every bus when firmware omits it.
    fn buses(&self) -> Result<(u8, u8), FdtError> {
        let Some(value) = self.bus_range else {
            return Ok((0, u8::MAX));
        };
        let value = value.get(..8).ok_or(FdtError::Malformed)?;
        let first = u8::try_from(cells(&value[..4])).map_err(|_| FdtError::Range)?;
        let last = u8::try_from(cells(&value[4..])).map_err(|_| FdtError::Range)?;
        Ok((first, last))
    }

    fn segment(&self) -> Result<u16, FdtError> {
        let Some(value) = self.domain else {
            return Ok(0);
        };
        u16::try_from(cell(value).ok_or(FdtError::Malformed)?).map_err(|_| FdtError::Range)
    }
}

/// Reads up to two big-endian cells as one number.
fn cells(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |value, &byte| value << 8 | u64::from(byte))
}

/// Reads a property that must be exactly one cell.
fn cell(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn be32(bytes: &[u8], offset: usize) -> Result<u32, FdtError> {
    let end = offset.checked_add(4).ok_or(FdtError::Truncated)?;
    let bytes: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(FdtError::Truncated)?
        .try_into()
        .map_err(|_| FdtError::Truncated)?;
    Ok(u32::from_be_bytes(bytes))
}

/// The offset just past a node's NUL-terminated, 4-byte-padded name.
fn skip_name(structs: &[u8], cursor: usize) -> Result<usize, FdtError> {
    let rest = structs.get(cursor..).ok_or(FdtError::Truncated)?;
    let nul = rest.iter().position(|&byte| byte == 0).ok_or(FdtError::Truncated)?;
    align(cursor + nul + 1)
}

/// A property name, from the strings block the header locates.
fn name(strings: &[u8], offset: usize) -> Result<&[u8], FdtError> {
    let rest = strings.get(offset..).ok_or(FdtError::Truncated)?;
    let nul = rest.iter().position(|&byte| byte == 0).ok_or(FdtError::Truncated)?;
    Ok(&rest[..nul])
}

fn align(offset: usize) -> Result<usize, FdtError> {
    offset.checked_add(3).map(|sum| sum & !3).ok_or(FdtError::Truncated)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::{DeviceTree, FdtError};

    /// Assembles a header around a struct and a strings block.
    fn blob(structs: &[u8], strings: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let offset = 40u32;
        bytes.extend_from_slice(&0xd00d_feedu32.to_be_bytes());
        bytes.extend_from_slice(
            &(offset + structs.len() as u32 + strings.len() as u32).to_be_bytes(),
        );
        bytes.extend_from_slice(&offset.to_be_bytes());
        bytes.extend_from_slice(&(offset + structs.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&offset.to_be_bytes());
        bytes.extend_from_slice(&17u32.to_be_bytes());
        bytes.extend_from_slice(&16u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&(strings.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&(structs.len() as u32).to_be_bytes());
        bytes.extend_from_slice(structs);
        bytes.extend_from_slice(strings);
        bytes
    }

    /// A root with 2/2 cells holding one node carrying `properties`.
    fn tree(properties: &[(&str, &[u8])]) -> Vec<u8> {
        let mut structs = Vec::new();
        let mut strings = Vec::new();
        let begin = |structs: &mut Vec<u8>, name: &str| {
            structs.extend_from_slice(&1u32.to_be_bytes());
            structs.extend_from_slice(name.as_bytes());
            structs.push(0);
            while structs.len() % 4 != 0 {
                structs.push(0);
            }
        };
        let property = |structs: &mut Vec<u8>, strings: &mut Vec<u8>, name: &str, value: &[u8]| {
            structs.extend_from_slice(&3u32.to_be_bytes());
            structs.extend_from_slice(&(value.len() as u32).to_be_bytes());
            structs.extend_from_slice(&(strings.len() as u32).to_be_bytes());
            structs.extend_from_slice(value);
            while structs.len() % 4 != 0 {
                structs.push(0);
            }
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        };

        begin(&mut structs, "");
        property(&mut structs, &mut strings, "#address-cells", &2u32.to_be_bytes());
        property(&mut structs, &mut strings, "#size-cells", &2u32.to_be_bytes());
        begin(&mut structs, "pci@30000000");
        for (name, value) in properties {
            property(&mut structs, &mut strings, name, value);
        }
        structs.extend_from_slice(&2u32.to_be_bytes());
        structs.extend_from_slice(&2u32.to_be_bytes());
        structs.extend_from_slice(&9u32.to_be_bytes());

        blob(&structs, &strings)
    }

    /// A `reg` of two address cells and two size cells, as QEMU `virt` emits.
    fn reg(base: u64, size: u64) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&base.to_be_bytes());
        bytes[8..].copy_from_slice(&size.to_be_bytes());
        bytes
    }

    fn bus_range(first: u32, last: u32) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[..4].copy_from_slice(&first.to_be_bytes());
        bytes[4..].copy_from_slice(&last.to_be_bytes());
        bytes
    }

    #[test]
    fn ecam_window_comes_from_the_pci_node() {
        let bytes = tree(&[
            ("compatible", b"pci-host-ecam-generic\0"),
            ("reg", &reg(0x3000_0000, 0x1000_0000)),
        ]);

        let space = DeviceTree::new(&bytes).expect("a well-formed blob").config_space();

        let space = space.expect("a well-formed PCI node");
        assert_eq!(space.span().expect("an ECAM span").start(), 0x3000_0000);
        assert_eq!(
            (space.first_bus(), space.last_bus()),
            (0, 0xff),
            "no bus-range means all buses"
        );
    }

    #[test]
    fn pci_domain_becomes_the_segment() {
        let bytes = tree(&[
            ("compatible", b"pci-host-ecam-generic\0"),
            ("reg", &reg(0x3000_0000, 0x1000_0000)),
            ("linux,pci-domain", &7u32.to_be_bytes()),
        ]);

        let space = DeviceTree::new(&bytes).expect("a well-formed blob").config_space();

        assert_eq!(space.expect("a well-formed PCI node").segment(), 7);
    }

    #[test]
    fn bus_range_narrows_the_last_bus() {
        let bytes = tree(&[
            ("compatible", b"pci-host-ecam-generic\0"),
            ("reg", &reg(0x3000_0000, 0x1000_0000)),
            ("bus-range", &bus_range(0, 15)),
        ]);

        let space = DeviceTree::new(&bytes).expect("a well-formed blob").config_space();

        assert_eq!(space.expect("a well-formed PCI node").last_bus(), 15);
    }

    #[test]
    fn window_too_small_for_the_bus_range_is_refused() {
        let bytes =
            tree(&[("compatible", b"pci-host-ecam-generic\0"), ("reg", &reg(0x3000_0000, 0x1000))]);

        let space = DeviceTree::new(&bytes).expect("a well-formed blob").config_space();

        assert_eq!(space, Err(FdtError::Range), "256 buses need 256 MiB of window");
    }

    #[test]
    fn tree_without_a_pci_node_is_missing() {
        let bytes = tree(&[("compatible", b"virtio,mmio\0"), ("reg", &reg(0x1000_1000, 0x1000))]);

        let space = DeviceTree::new(&bytes).expect("a well-formed blob").config_space();

        assert_eq!(space, Err(FdtError::Missing));
    }

    #[test]
    fn a_null_device_tree_pointer_is_refused_without_a_load() {
        // SAFETY: zero is the one address `config_space_at` promises to answer
        // without dereferencing anything.
        let space = unsafe { super::config_space_at(0) };

        assert_eq!(space, Err(FdtError::Missing), "a hart entered without a tree");
    }

    #[test]
    fn a_device_tree_in_memory_yields_its_ecam_window() {
        let bytes = tree(&[
            ("compatible", b"pci-host-ecam-generic\0"),
            ("reg", &reg(0x3000_0000, 0x1000_0000)),
        ]);

        // SAFETY: the blob is live for the call and is not mutated during it.
        let space = unsafe { super::config_space_at(bytes.as_ptr() as usize) };

        assert_eq!(
            space.expect("a well-formed PCI node").span().expect("a span").start(),
            0x3000_0000
        );
    }

    #[test]
    fn bad_magic_is_refused() {
        let mut bytes = tree(&[("compatible", b"pci-host-ecam-generic\0")]);

        bytes[0] ^= 0xff;

        assert_eq!(DeviceTree::new(&bytes).err(), Some(FdtError::Magic));
    }

    #[test]
    fn truncated_struct_block_is_refused() {
        let mut bytes = tree(&[
            ("compatible", b"pci-host-ecam-generic\0"),
            ("reg", &reg(0x3000_0000, 0x1000_0000)),
        ]);
        // Drop the two closing tokens and the end token from `size_dt_struct`,
        // so the walk runs out of block with the node still open.
        let short = u32::from_be_bytes(bytes[36..40].try_into().expect("four bytes")) - 12;

        bytes[36..40].copy_from_slice(&short.to_be_bytes());

        let space = DeviceTree::new(&bytes).expect("an intact header").config_space();
        assert_eq!(space, Err(FdtError::Truncated), "the last token reaches past the block");
    }
}
