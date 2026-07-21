//! Everything the enumeration path refuses to guess about.

/// Why a configuration-space request could not be answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A bus, device, or function number PCI cannot encode.
    Address,
    /// A register offset outside the 4 KiB configuration space, or unaligned.
    Offset,
    /// No function answered at that address.
    Absent,
    /// The header type has no such base address register, or it reads as zero
    /// width because the device does not implement it.
    Bar,
    /// The register decodes I/O space, or is the upper half of a 64-bit pair.
    /// Neither is a window this kernel will map.
    NotMemory,
    /// The function does not implement the capability that was asked for.
    Missing,
    /// The capability list points outside the header or back into itself.
    Malformed,
    /// A vector index outside the device's MSI-X table.
    Vector,
}
