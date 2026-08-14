//! Host-testable `satp` field encoding: [`Mode`], and the [`Tag`] beside it.
//!
//! Both fields are discoverable rather than declared, which is why nothing here
//! asserts a width:
//!
//! - **MODE.** A write naming a mode the hart does not implement has no effect,
//!   so whatever reads back is the answer. [`Mode::WIDEST`] is the probe order;
//!   [`Mode::level`] is the depth the page-table code then walks.
//! - **ASID.** The specification leaves the implemented bit count UNSPECIFIED
//!   and the field WARL, so [`Tag::width`] writes sixteen ones and counts what
//!   stayed.

/// A `satp` MODE the kernel is willing to run in.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Mode {
    /// Three levels, 39-bit addresses: 512 GiB.
    Sv39,
    /// Four levels, 48-bit addresses: 256 TiB.
    Sv48,
    /// Five levels, 57-bit addresses: 128 PiB.
    Sv57,
}

impl Mode {
    /// Probe order. Widest first, because the first write that takes wins.
    pub const WIDEST: [Self; 3] = [Self::Sv57, Self::Sv48, Self::Sv39];

    /// Bits 63:60 of `satp`.
    const SHIFT: u32 = 60;

    /// The MODE field, already shifted where `satp` wants it.
    pub const fn field(self) -> u64 {
        (self.code() as u64) << Self::SHIFT
    }

    /// The encoding the privileged specification gives this mode.
    const fn code(self) -> u8 {
        match self {
            Self::Sv39 => 8,
            Self::Sv48 => 9,
            Self::Sv57 => 10,
        }
    }

    /// Decodes the MODE a `satp` value reads back with. `None` covers Bare and
    /// every encoding this kernel does not build tables for, which answers the
    /// probe's only question the same way: not the mode that was written.
    pub const fn of(satp: u64) -> Option<Self> {
        Some(match satp >> Self::SHIFT {
            8 => Self::Sv39,
            9 => Self::Sv48,
            10 => Self::Sv57,
            _ => return None,
        })
    }

    /// How many virtual address bits translation resolves.
    pub const fn bits(self) -> u32 {
        match self {
            Self::Sv39 => 39,
            Self::Sv48 => 48,
            Self::Sv57 => 57,
        }
    }

    /// The level the root table sits at.
    ///
    /// Level `n` of a virtual address is the nine bits at `12 + 9 * n`, so the
    /// root of a mode resolving `bits` addresses is at `(bits - 12) / 9 - 1`.
    pub const fn level(self) -> usize {
        (self.bits() as usize - 12) / 9 - 1
    }

    /// The lowercase name the boot marker prints.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sv39 => "sv39",
            Self::Sv48 => "sv48",
            Self::Sv57 => "sv57",
        }
    }

    /// A virtual address only this mode can translate, so that the width is
    /// proven by a translation rather than asserted.
    ///
    /// Well inside the lower canonical half, so the sign extension a wider mode
    /// demands above `bits() - 1` is satisfied by those bits being zero.
    pub const fn probe_va(self) -> usize {
        match self {
            // Below 512 GiB: Sv39 has nowhere else to put it.
            Self::Sv39 => 0x2000_0000,
            // 64 TiB and 16 PiB: unreachable one mode down.
            Self::Sv48 => 1 << 46,
            Self::Sv57 => 1 << 54,
        }
    }
}

/// The ASID field of `satp`: bits 59:44, sixteen bits at most on RV64.
pub struct Tag;

impl Tag {
    /// Bits 59:44 of `satp`.
    const SHIFT: u32 = 44;

    /// Every bit the field could possibly hold, which is what the probe writes.
    pub const MASK: u64 = 0xffff << Self::SHIFT;

    /// A tag, already shifted where `satp` wants it.
    pub const fn field(value: u16) -> u64 {
        (value as u64) << Self::SHIFT
    }

    /// The tag a `satp` value reads back with.
    pub const fn of(satp: u64) -> u16 {
        ((satp & Self::MASK) >> Self::SHIFT) as u16
    }

    /// How many tag bits a hart implements, given a `satp` read back after
    /// [`MASK`](Self::MASK) was written into it.
    ///
    /// Only the low contiguous run counts: the field is WARL, so a hart may
    /// leave a high bit set that it does not decode, and a tag whose low bits
    /// alias another domain's is worse than no tag.
    pub const fn width(live: u64) -> u32 {
        Self::of(live).trailing_ones()
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, Tag};

    #[test]
    fn field_round_trips_through_satp() {
        for mode in Mode::WIDEST {
            assert_eq!(Mode::of(mode.field() | 0x1234), Some(mode));
        }
    }

    #[test]
    fn bare_and_unknown_modes_decode_to_nothing() {
        assert_eq!(Mode::of(0), None);
        // Sv32 on rv64, and the reserved encodings above Sv57.
        assert_eq!(Mode::of(1 << 60), None);
        assert_eq!(Mode::of(11 << 60), None);
        assert_eq!(Mode::of(15 << 60), None);
    }

    #[test]
    fn levels_match_specified_widths() {
        assert_eq!(Mode::Sv39.level(), 2);
        assert_eq!(Mode::Sv48.level(), 3);
        assert_eq!(Mode::Sv57.level(), 4);
    }

    #[test]
    fn probe_order_is_widest_first() {
        for pair in Mode::WIDEST.windows(2) {
            assert!(pair[0] > pair[1], "{pair:?} is not descending");
        }
    }

    #[test]
    fn tag_round_trips_beside_mode() {
        let satp = Mode::Sv57.field() | Tag::field(0xbeef) | 0x8_0000;

        assert_eq!(Tag::of(satp), 0xbeef);
        assert_eq!(Mode::of(satp), Some(Mode::Sv57), "the tag disturbed the mode");
        assert_eq!(
            satp & !(Tag::MASK | Mode::Sv57.field()),
            0x8_0000,
            "the tag disturbed the root"
        );
    }

    #[test]
    fn probe_counts_stuck_bits() {
        // What QEMU's `virt` hart answers with, and what a narrower one would.
        assert_eq!(Tag::width(Tag::MASK), 16);
        assert_eq!(Tag::width(Tag::field(0x1ff)), 9);
        assert_eq!(Tag::width(0), 0, "a hart with no field was read as having one");
    }

    #[test]
    fn hole_in_field_ends_count() {
        // WARL lets a hart keep a bit it does not decode; anything above a gap
        // would hand two domains tags that alias in the bits that do decode.
        assert_eq!(Tag::width(Tag::field(0b1000_1111)), 4);
    }

    #[test]
    fn probe_address_needs_naming_mode() {
        for mode in Mode::WIDEST {
            let va = mode.probe_va();
            assert!(va < (1 << (mode.bits() - 1)), "{} cannot translate {va:#x}", mode.name());
            if mode != Mode::Sv39 {
                let narrower = Mode::WIDEST[Mode::WIDEST.len() - 1];
                assert!(va > (1 << narrower.bits()), "{va:#x} is reachable below {}", mode.name());
            }
        }
    }
}
