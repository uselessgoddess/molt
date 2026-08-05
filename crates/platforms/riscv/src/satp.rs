//! Host-testable `satp` MODE encoding.
//!
//! How wide the supervisor's address space is comes down to four bits in one
//! CSR, and the privileged specification makes those bits discoverable instead
//! of declared: a write naming a MODE the hart does not implement has no effect
//! at all, so whatever reads back afterwards is the answer. [`Mode::WIDEST`] is
//! the order that probe runs in and [`Mode::level`] is the depth the page-table
//! code walks; everything else about a mode follows from those two.

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

    /// Decodes the MODE a `satp` value reads back with.
    ///
    /// `None` covers Bare and every encoding this kernel does not build tables
    /// for, which is the same answer to the only question the probe asks: is
    /// this the mode that was written?
    pub const fn from_satp(satp: u64) -> Option<Self> {
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

    /// A virtual address only this mode can translate, for a probe that proves
    /// the width rather than asserting it.
    ///
    /// It is two levels above the narrowest mode's ceiling and well inside the
    /// lower canonical half, so the sign extension a wider mode demands of bits
    /// above `bits() - 1` is satisfied by their being zero.
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

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn field_round_trips_through_satp() {
        for mode in Mode::WIDEST {
            assert_eq!(Mode::from_satp(mode.field() | 0x1234), Some(mode));
        }
    }

    #[test]
    fn bare_and_unknown_modes_decode_to_nothing() {
        assert_eq!(Mode::from_satp(0), None);
        // Sv32 on rv64, and the reserved encodings above Sv57.
        assert_eq!(Mode::from_satp(1 << 60), None);
        assert_eq!(Mode::from_satp(11 << 60), None);
        assert_eq!(Mode::from_satp(15 << 60), None);
    }

    #[test]
    fn levels_match_the_specified_widths() {
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
    fn probe_addresses_need_the_mode_that_names_them() {
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
