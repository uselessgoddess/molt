//! The 32-bit window every other module reads through.

use crate::address::Address;

/// Vendor and device identifiers, one word.
pub const ID: u16 = 0x00;
/// Command in the low half, status in the high half.
pub const COMMAND: u16 = 0x04;
/// Revision in the low byte, class code in the upper three.
pub const CLASS: u16 = 0x08;
/// Cache line size, latency, header type, and BIST.
pub const HEADER: u16 = 0x0c;
/// The first base address register.
pub const BAR: u16 = 0x10;
/// Offset of the first capability, in the low byte.
pub const CAPABILITIES: u16 = 0x34;

/// The vendor identifier a bus reports where nothing answers.
pub const ABSENT: u16 = 0xffff;
/// Status bit that says the capability list is present.
pub const HAS_CAPABILITIES: u32 = 1 << (16 + 4);
/// Header-type bit that says the device implements more than function zero.
pub const MULTIFUNCTION: u32 = 0x80 << 16;

/// Configuration space of one segment, addressed a word at a time.
///
/// Configuration space is only ever defined for aligned 32-bit accesses, so
/// that is the whole interface: the byte and half-word views every register
/// description is written in are shifts, and belong here rather than in each
/// caller. Reads of an address no function answers must return all ones, the
/// value the bus itself supplies, so that absence needs no separate channel.
pub trait Config {
    /// Reads the register at `offset`, which must be word-aligned and inside
    /// the 4 KiB window.
    fn read(&self, at: Address, offset: u16) -> u32;

    /// Writes the register at `offset`, under the same bounds as [`read`].
    ///
    /// [`read`]: Config::read
    fn write(&self, at: Address, offset: u16, value: u32);

    /// The bus numbers this window covers, inclusive.
    ///
    /// Enumeration is a sweep of exactly this range: it is what the platform
    /// mapped and audited, and it needs no bridge to have been programmed
    /// correctly before it can be trusted.
    fn buses(&self) -> (u8, u8);
}

/// Reads the half-word at `offset`, which need only be two-byte aligned.
pub fn read16<C: Config + ?Sized>(config: &C, at: Address, offset: u16) -> u16 {
    (config.read(at, offset & !3) >> (8 * (offset & 2))) as u16
}

/// Reads the byte at `offset`.
pub fn read8<C: Config + ?Sized>(config: &C, at: Address, offset: u16) -> u8 {
    (config.read(at, offset & !3) >> (8 * (offset & 3))) as u8
}

/// Rewrites the half-word at `offset`, leaving the other half of the word as
/// it was read. Write-one-to-clear status bits are read back and rewritten,
/// which clears whatever was set — the same hazard a byte-granular bus has, and
/// the reason nothing here writes a half of a word it did not mean to touch.
pub fn write16<C: Config + ?Sized>(config: &C, at: Address, offset: u16, value: u16) {
    let shift = 8 * (offset & 2);
    let word = config.read(at, offset & !3) & !(0xffff << shift);
    config.write(at, offset & !3, word | u32::from(value) << shift);
}
