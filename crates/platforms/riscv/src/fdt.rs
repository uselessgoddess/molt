//! Just enough of the flattened device tree to find where a bus lives.
//!
//! RISC-V has no equivalent of the PC's architectural addresses: there is no
//! fixed configuration base, no fixed UART, and nothing a kernel may assume.
//! What there is instead is a device tree, which the previous stage leaves a
//! pointer to in `a1`, and which says where everything is. So the kernel that
//! hardcodes the emulator's numbers is the kernel that writes into whatever
//! lives at those numbers on a board that chose differently.
//!
//! This is a reader, not a parser: nothing is allocated and nothing is kept.
//! It walks the structure block once per question asked, which costs a few
//! microseconds at boot and saves a copy of the whole tree. Everything below
//! [`Fdt::new`] is ordinary safe code over a byte slice, so it is exercised on
//! the host against hand-built trees rather than only on the board.

/// Big-endian magic every flattened tree starts with.
const MAGIC: u32 = 0xd00d_feed;

/// The format has been at version 17 since before RISC-V existed; an older
/// header does not place the structure block where this reader looks.
const VERSION: u32 = 17;

/// Longest tree this reader will look at. QEMU's is a few kilobytes.
const LIMIT: usize = 1024 * 1024;

/// How deeply nodes may nest before the reader gives up. A bus is three levels
/// down on every board that exists; this is a bound on a loop, not a budget.
const DEPTH: usize = 16;

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

/// Cells the specification says to assume where a parent says nothing.
const DEFAULT_ADDRESS_CELLS: u64 = 2;
const DEFAULT_SIZE_CELLS: u64 = 1;

/// One address range a node's `reg` property describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Region {
    pub base: u64,
    pub size: u64,
}

/// What the board calls a memory-mapped configuration space this kernel knows
/// how to read. A host bridge that answers to something else answers to a
/// different set of registers, and enumerating it as if it did not would be a
/// guess.
pub const ECAM: &str = "pci-host-ecam-generic";

/// Where the board puts configuration space, according to the tree at
/// `address`, or `None` where it has no bus this kernel can read.
///
/// # Safety
///
/// `address` must be the device-tree pointer the previous stage passed in, and
/// the tree behind it must still be intact. Everything read out of it is copied
/// here, so nothing borrows the tree once this returns — which matters because
/// the tree sits in RAM the kernel is about to start allocating frames from.
pub unsafe fn configuration(address: usize) -> Option<Region> {
    // SAFETY: the caller vouched for the pointer and for the tree behind it.
    unsafe { Fdt::at(address) }?.find(ECAM)?.region(0)
}

/// A flattened device tree, borrowed where it was left.
#[derive(Clone, Copy)]
pub struct Fdt<'d> {
    structure: &'d [u8],
    strings: &'d [u8],
}

impl<'d> Fdt<'d> {
    /// Reads the header at `address` and borrows the tree behind it.
    ///
    /// # Safety
    ///
    /// `address` must be where the previous stage left a flattened device tree,
    /// in memory that stays readable and unmodified for `'static`. On this
    /// platform that is the pointer OpenSBI passes in `a1`, which points into
    /// firmware-reserved memory the kernel never maps writable.
    pub unsafe fn at(address: usize) -> Option<Fdt<'static>> {
        if address == 0 || address % 8 != 0 {
            return None;
        }
        // SAFETY: the caller vouched for a tree at `address`; a header is the
        // first 40 bytes of one, and this reads no further until it checks out.
        let header = unsafe { core::slice::from_raw_parts(address as *const u8, 40) };
        let total = be32(header, 4)? as usize;
        if be32(header, 0)? != MAGIC || !(40..=LIMIT).contains(&total) {
            return None;
        }
        // SAFETY: the magic matched and the tree declares its own length, which
        // the caller's guarantee covers.
        Fdt::new(unsafe { core::slice::from_raw_parts(address as *const u8, total) })
    }

    /// Borrows a tree already in memory, checking everything the header claims.
    pub fn new(bytes: &'d [u8]) -> Option<Fdt<'d>> {
        let total = be32(bytes, 4)? as usize;
        if be32(bytes, 0)? != MAGIC || be32(bytes, 20)? != VERSION || total > bytes.len() {
            return None;
        }
        let structure = block(bytes, be32(bytes, 8)? as usize, be32(bytes, 36)? as usize)?;
        let strings = block(bytes, be32(bytes, 12)? as usize, be32(bytes, 32)? as usize)?;
        (structure.len() % 4 == 0).then_some(Fdt { structure, strings })
    }

    /// The first node claiming to be `compatible` with the given string.
    ///
    /// Nodes are matched on what they say they are rather than on where they
    /// sit, because a name is a label and `compatible` is a contract: a bus
    /// that answers to `pci-host-ecam-generic` is one this kernel knows how to
    /// read, whatever the board decided to call it.
    pub fn find(&self, compatible: &str) -> Option<Node<'d>> {
        let mut walk = Walk::new(self.structure);
        let mut node: Option<Node<'d>> = None;
        while let Some(token) = walk.next() {
            match token {
                // A node's properties all precede its first child, so either
                // token that follows them settles the question.
                Token::Begin => {
                    if let Some(node) = node.take().filter(|node| node.matched) {
                        return Some(node);
                    }
                    node = Some(Node {
                        address_cells: walk.address_cells(),
                        size_cells: walk.size_cells(),
                        reg: None,
                        matched: false,
                    })
                }
                Token::Prop { name, value } => match self.name(name) {
                    // Cell widths are a statement about children, so they are
                    // recorded in the cursor rather than in the node.
                    Some(b"#address-cells") => walk.declare_address_cells(cells(value)),
                    Some(b"#size-cells") => walk.declare_size_cells(cells(value)),
                    Some(b"compatible") => {
                        if let Some(node) = node.as_mut() {
                            node.matched = value
                                .split(|byte| *byte == 0)
                                .any(|entry| entry == compatible.as_bytes());
                        }
                    }
                    Some(b"reg") => {
                        if let Some(node) = node.as_mut() {
                            node.reg = Some(value);
                        }
                    }
                    _ => {}
                },
                Token::End => {
                    if let Some(node) = node.take().filter(|node| node.matched) {
                        return Some(node);
                    }
                }
            }
        }
        None
    }

    /// The name a property's offset into the strings block refers to.
    fn name(&self, at: usize) -> Option<&'d [u8]> {
        let rest = self.strings.get(at..)?;
        Some(&rest[..rest.iter().position(|byte| *byte == 0)?])
    }
}

/// A node the tree matched, and the parts of it worth reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node<'d> {
    address_cells: u64,
    size_cells: u64,
    reg: Option<&'d [u8]>,
    matched: bool,
}

impl Node<'_> {
    /// The `index`th range of the node's `reg`, decoded with the cell widths
    /// its parent declared.
    ///
    /// A cell is four bytes and the count is the parent's to choose, so a
    /// reader that assumes 64-bit addresses is a reader that works on one
    /// board. Anything wider than 64 bits is refused rather than truncated.
    pub fn region(&self, index: usize) -> Option<Region> {
        let (address, size) = (self.address_cells as usize, self.size_cells as usize);
        if address == 0 || address > 2 || size > 2 {
            return None;
        }
        let stride = (address + size) * 4;
        let entry = self.reg?.get(index * stride..(index + 1) * stride)?;
        Some(Region { base: cells(&entry[..address * 4]), size: cells(&entry[address * 4..]) })
    }
}

/// A byte range the header names, checked to be inside the tree.
fn block(bytes: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    bytes.get(at..at.checked_add(len)?)
}

/// One step of a walk over the structure block.
enum Token<'d> {
    Begin,
    Prop { name: usize, value: &'d [u8] },
    End,
}

/// A cursor over the structure block that remembers the cell widths in force.
///
/// Cell counts are inherited: a node's `reg` is decoded with the widths its
/// *parent* declared, so the walk carries one entry per open node and a child
/// starts from a copy of its parent's.
struct Walk<'d> {
    bytes: &'d [u8],
    at: usize,
    depth: usize,
    address_cells: [u64; DEPTH],
    size_cells: [u64; DEPTH],
}

impl<'d> Walk<'d> {
    fn new(bytes: &'d [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            depth: 0,
            address_cells: [DEFAULT_ADDRESS_CELLS; DEPTH],
            size_cells: [DEFAULT_SIZE_CELLS; DEPTH],
        }
    }

    /// Cell widths the node being read must decode its `reg` with: its
    /// parent's, which is the entry one level up from its own.
    fn address_cells(&self) -> u64 {
        self.address_cells[self.depth.saturating_sub(1)]
    }

    fn size_cells(&self) -> u64 {
        self.size_cells[self.depth.saturating_sub(1)]
    }

    /// Records what the open node says its children's addresses are made of.
    fn declare_address_cells(&mut self, cells: u64) {
        self.address_cells[self.depth] = cells;
    }

    fn declare_size_cells(&mut self, cells: u64) {
        self.size_cells[self.depth] = cells;
    }

    fn next(&mut self) -> Option<Token<'d>> {
        loop {
            let token = be32(self.bytes, self.at)?;
            self.at += 4;
            match token {
                NOP => continue,
                END => return None,
                BEGIN_NODE => {
                    // The name is null-terminated and padded to a cell.
                    let rest = self.bytes.get(self.at..)?;
                    self.at += align(rest.iter().position(|byte| *byte == 0)? + 1);
                    if self.depth + 1 >= DEPTH {
                        return None;
                    }
                    // A child starts out seeing what its parent sees, and
                    // overrides it only by saying so.
                    self.address_cells[self.depth + 1] = self.address_cells[self.depth];
                    self.size_cells[self.depth + 1] = self.size_cells[self.depth];
                    self.depth += 1;
                    return Some(Token::Begin);
                }
                END_NODE => {
                    self.depth = self.depth.checked_sub(1)?;
                    return Some(Token::End);
                }
                PROP => {
                    let len = be32(self.bytes, self.at)? as usize;
                    let name = be32(self.bytes, self.at + 4)? as usize;
                    let value = self.bytes.get(self.at + 8..self.at + 8 + len)?;
                    self.at += 8 + align(len);
                    return Some(Token::Prop { name, value });
                }
                _ => return None,
            }
        }
    }
}

/// Rounds a length up to a whole cell, the way the format pads.
const fn align(len: usize) -> usize {
    len.div_ceil(4) * 4
}

/// A big-endian address or size, one to two cells wide.
fn cells(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |value, byte| value << 8 | u64::from(*byte))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec;
    use std::vec::Vec;

    use super::{Fdt, Region};

    /// A tree under construction, written the way a bootloader writes one.
    struct Tree {
        structure: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Tree {
        fn new() -> Self {
            Self { structure: Vec::new(), strings: Vec::new() }
        }

        fn begin(&mut self, name: &str) -> &mut Self {
            self.structure.extend_from_slice(&1u32.to_be_bytes());
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
            self
        }

        fn end(&mut self) -> &mut Self {
            self.structure.extend_from_slice(&2u32.to_be_bytes());
            self
        }

        fn prop(&mut self, name: &str, value: &[u8]) -> &mut Self {
            let at = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.structure.extend_from_slice(&3u32.to_be_bytes());
            self.structure.extend_from_slice(&(value.len() as u32).to_be_bytes());
            self.structure.extend_from_slice(&at.to_be_bytes());
            self.structure.extend_from_slice(value);
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
            self
        }

        /// A property made of whole big-endian cells, the way `reg` is written.
        fn cells(&mut self, name: &str, values: &[u32]) -> &mut Self {
            let bytes: Vec<u8> = values.iter().flat_map(|value| value.to_be_bytes()).collect();
            self.prop(name, &bytes)
        }

        /// A null-terminated string property, the way `compatible` is written.
        fn text(&mut self, name: &str, value: &str) -> &mut Self {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            self.prop(name, &bytes)
        }

        fn finish(&mut self) -> Vec<u8> {
            self.structure.extend_from_slice(&9u32.to_be_bytes());
            // Header, then the empty reservation block, then the two blocks the
            // header points at.
            let (structure, strings) = (56, 56 + self.structure.len());
            let total = strings + self.strings.len();
            let mut bytes = vec![0u8; 56];
            bytes[0..4].copy_from_slice(&0xd00d_feedu32.to_be_bytes());
            bytes[4..8].copy_from_slice(&(total as u32).to_be_bytes());
            bytes[8..12].copy_from_slice(&(structure as u32).to_be_bytes());
            bytes[12..16].copy_from_slice(&(strings as u32).to_be_bytes());
            bytes[16..20].copy_from_slice(&40u32.to_be_bytes());
            bytes[20..24].copy_from_slice(&17u32.to_be_bytes());
            bytes[24..28].copy_from_slice(&16u32.to_be_bytes());
            bytes[32..36].copy_from_slice(&(self.strings.len() as u32).to_be_bytes());
            bytes[36..40].copy_from_slice(&(self.structure.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&self.structure);
            bytes.extend_from_slice(&self.strings);
            bytes
        }
    }

    /// What the QEMU `virt` board hands over, with everything this reader does
    /// not look at left out.
    fn board() -> Vec<u8> {
        Tree::new()
            .begin("")
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .begin("soc")
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .text("compatible", "simple-bus")
            .begin("serial@10000000")
            .text("compatible", "ns16550a")
            .cells("reg", &[0, 0x1000_0000, 0, 0x100])
            .end()
            .begin("pci@30000000")
            .text("compatible", "pci-host-ecam-generic")
            .cells("reg", &[0, 0x3000_0000, 0, 0x1000_0000])
            .cells("bus-range", &[0, 0xff])
            .end()
            .end()
            .end()
            .finish()
    }

    #[test]
    fn a_bus_is_found_by_what_it_claims_to_be() {
        let bytes = board();
        let fdt = Fdt::new(&bytes).expect("a well-formed tree");

        let bus = fdt.find("pci-host-ecam-generic").expect("the board's bus");
        assert_eq!(bus.region(0), Some(Region { base: 0x3000_0000, size: 0x1000_0000 }));
        assert_eq!(bus.region(1), None, "a second range was invented");
    }

    #[test]
    fn nodes_are_told_apart_by_compatible_rather_than_by_order() {
        let bytes = board();
        let fdt = Fdt::new(&bytes).expect("a well-formed tree");

        let uart = fdt.find("ns16550a").expect("the board's console");
        assert_eq!(uart.region(0), Some(Region { base: 0x1000_0000, size: 0x100 }));
        assert_eq!(fdt.find("pci-host-cam-generic"), None, "a bus that is not there was found");
    }

    #[test]
    fn a_range_is_decoded_with_the_widths_its_parent_declared() {
        // The same four cells mean two 64-bit numbers under one parent and four
        // 32-bit ones under another; a reader that assumes is a reader that is
        // right on one board.
        let bytes = Tree::new()
            .begin("")
            .cells("#address-cells", &[1])
            .cells("#size-cells", &[1])
            .begin("pci@30000000")
            .text("compatible", "pci-host-ecam-generic")
            .cells("reg", &[0x3000_0000, 0x1000_0000, 0x4000_0000, 0x4000_0000])
            .end()
            .end()
            .finish();
        let fdt = Fdt::new(&bytes).expect("a well-formed tree");

        let bus = fdt.find("pci-host-ecam-generic").expect("the tree's bus");
        assert_eq!(bus.region(0), Some(Region { base: 0x3000_0000, size: 0x1000_0000 }));
        assert_eq!(bus.region(1), Some(Region { base: 0x4000_0000, size: 0x4000_0000 }));
    }

    #[test]
    fn a_node_that_has_children_is_still_the_node_that_matched() {
        let bytes = Tree::new()
            .begin("")
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .begin("pci@30000000")
            .text("compatible", "pci-host-ecam-generic")
            .cells("reg", &[0, 0x3000_0000, 0, 0x1000_0000])
            .begin("ethernet@0")
            .text("compatible", "virtio,pci")
            .end()
            .end()
            .end()
            .finish();
        let fdt = Fdt::new(&bytes).expect("a well-formed tree");

        let bus = fdt.find("pci-host-ecam-generic").expect("the tree's bus");
        assert_eq!(bus.region(0), Some(Region { base: 0x3000_0000, size: 0x1000_0000 }));
    }

    #[test]
    fn what_is_not_a_tree_is_refused_rather_than_read() {
        let bytes = board();

        let mut wrong_magic = bytes.clone();
        wrong_magic[3] = 0;
        assert!(Fdt::new(&wrong_magic).is_none(), "a header without the magic was believed");

        let mut wrong_version = bytes.clone();
        wrong_version[23] = 16;
        assert!(
            Fdt::new(&wrong_version).is_none(),
            "a layout this reader cannot read was believed"
        );

        assert!(Fdt::new(&bytes[..bytes.len() - 4]).is_none(), "a truncated tree was believed");
        assert!(Fdt::new(&[]).is_none(), "nothing at all was believed");
    }

    #[test]
    fn a_property_running_past_the_block_ends_the_walk() {
        // The last property's length is stretched past everything after it: a
        // reader that trusted it would hand out a slice of the strings block.
        let mut bytes = board();
        let end = bytes.len();
        let at = bytes
            .windows(4)
            .rposition(|window| window == 3u32.to_be_bytes())
            .expect("a property token");
        bytes[at + 4..at + 8].copy_from_slice(&(end as u32).to_be_bytes());
        let fdt = Fdt::new(&bytes).expect("a header that still checks out");

        assert_eq!(fdt.find("pci-host-ecam-generic"), None, "a property past the block was read");
    }
}
