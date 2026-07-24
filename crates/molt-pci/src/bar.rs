//! Base address registers, and finding out how large they are.
//!
//! A BAR does not report its size; it reports which of its address bits are
//! writable, and the only way to ask is to write ones into it and read back
//! what stuck. That is destructive — for the duration the register names a
//! decode window at whatever address the ones landed on — so [`Function::bar`]
//! turns decode off first and puts the original value back afterwards, on
//! every path including the failing ones.
//!
//! The arithmetic that turns "these bits are writable" into a base and a
//! length is [`decode`], which is a pure function of two register values. That
//! split is deliberate: the destructive half needs real hardware to mean
//! anything, and the half that is easy to get wrong needs none.

use molt_arch::memory::{Error, Span};

use crate::PciError;
use crate::function::{Command, Function};

/// The first BAR's offset, and how many a header type has.
const FIRST: u64 = 0x10;
const DEVICE_BARS: u8 = 6;
const BRIDGE_BARS: u8 = 2;

/// Bit 0 selects the address space; the rest of the low bits describe it.
const IO: u32 = 1 << 0;
const WIDE: u32 = 0b10 << 1;
const TYPE: u32 = 0b11 << 1;
const PREFETCHABLE: u32 = 1 << 3;

/// What kind of window a BAR asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarKind {
    /// A memory window. `wide` means the BAR is 64 bits and consumed the next
    /// register as its upper half.
    Memory { prefetchable: bool, wide: bool },
    /// A port-I/O window.
    Io,
}

/// One decoded base address register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bar {
    index: u8,
    base: u64,
    len: u64,
    kind: BarKind,
}

impl Bar {
    pub const fn index(self) -> u8 {
        self.index
    }

    /// The physical address firmware assigned the window.
    pub const fn base(self) -> u64 {
        self.base
    }

    pub const fn bytes(self) -> u64 {
        self.len
    }

    pub const fn kind(self) -> BarKind {
        self.kind
    }

    pub const fn is_memory(self) -> bool {
        matches!(self.kind, BarKind::Memory { .. })
    }

    /// The frames the window occupies, rounded out to whole frames.
    ///
    /// Rounding out is what makes a BAR mappable, and it is also the risk: a
    /// BAR smaller than a frame shares that frame with whatever the bus put
    /// beside it. [`Inventory::device`](molt_arch::memory::Inventory::device)
    /// is what refuses a span that is not all device memory, which is why this
    /// returns a span rather than a mapping.
    pub fn span(self) -> Result<Span, Error> {
        let frame = molt_arch::FRAME_SIZE;
        if !self.is_memory() {
            return Err(Error::Kind);
        }
        let start = self.base & !(frame - 1);
        let end = self.base.checked_add(self.len).ok_or(Error::Range)?;
        Span::new(start, end.next_multiple_of(frame))
    }
}

fn decode(index: u8, original: u32, probed: u32, upper: Option<(u32, u32)>) -> Option<Bar> {
    let (kind, reserved) = if original & IO != 0 {
        (BarKind::Io, 0b11)
    } else {
        let wide = original & TYPE == WIDE;
        (BarKind::Memory { prefetchable: original & PREFETCHABLE != 0, wide }, 0b1111)
    };

    let (base, writable, bits) = match upper {
        Some((high, probed_high)) => (
            (high as u64) << 32 | (original & !reserved) as u64,
            (probed_high as u64) << 32 | (probed & !reserved) as u64,
            64,
        ),
        None => ((original & !reserved) as u64, (probed & !reserved) as u64, 32),
    };

    if writable == 0 {
        return None;
    }
    // The length is the value of the lowest writable bit.
    let len = match bits {
        64 => (!writable).wrapping_add(1),
        _ => (!(writable as u32)).wrapping_add(1) as u64,
    };
    if len == 0 {
        return None;
    }
    Some(Bar { index, base, len, kind })
}

impl Function<'_> {
    /// Sizes BAR `index`, or reports that the function does not implement one.
    ///
    /// Decode is disabled for the probe and restored afterwards, so this is
    /// safe to call on a running device but not free: the device cannot be
    /// reached while it runs.
    pub fn bar(&mut self, index: u8) -> Result<Option<Bar>, PciError> {
        let offset = self.bar_offset(index)?;
        let command = self.command()?;
        self.set_command(command.without(Command::MEMORY.with(Command::IO)))?;
        let sized = self.probe_bar(index, offset);
        // Restored even when the probe failed: leaving decode off would strand
        // a device that was working before anyone asked about its BARs.
        self.set_command(command)?;
        sized
    }

    /// The configuration-space offset of BAR `index` for this header type.
    fn bar_offset(&self, index: u8) -> Result<u64, PciError> {
        let count = match self.header_type() {
            0 => DEVICE_BARS,
            1 => BRIDGE_BARS,
            _ => return Err(PciError::Layout),
        };
        if index >= count {
            return Err(PciError::Layout);
        }
        Ok(FIRST + index as u64 * 4)
    }

    /// Writes all-ones into the register, reads the mask back, and restores it.
    fn probe_bar(&self, index: u8, offset: u64) -> Result<Option<Bar>, PciError> {
        let window = self.window();
        let original = window.read_u32(offset)?;
        window.write_u32(offset, u32::MAX)?;
        let probed = window.read_u32(offset)?;
        window.write_u32(offset, original)?;

        let wide = original & IO == 0 && original & TYPE == WIDE;
        if !wide {
            return Ok(decode(index, original, probed, None));
        }

        // A 64-bit BAR spends the next register on its upper half, so it cannot
        // be the last one this header type has.
        let upper = self.bar_offset(index + 1)?;
        let high = window.read_u32(upper)?;
        window.write_u32(upper, u32::MAX)?;
        let probed_high = window.read_u32(upper)?;
        window.write_u32(upper, high)?;
        Ok(decode(index, original, probed, Some((high, probed_high))))
    }
}

#[cfg(test)]
mod tests {
    use super::{Bar, BarKind, decode};
    use crate::fake::Space;
    use crate::function::{Command, Function};
    use crate::{Address, PciError};

    #[test]
    fn bar32_reports_size() {
        let bar = decode(0, 0xfebc_0000, 0xffff_0000, None).expect("an implemented BAR");

        assert_eq!(bar.base(), 0xfebc_0000);
        assert_eq!(bar.bytes(), 0x1_0000);
        assert_eq!(bar.kind(), BarKind::Memory { prefetchable: false, wide: false });
    }

    #[test]
    fn bar64_joins_both_halves() {
        let bar = decode(2, 0x0000_000c, 0xffc0_0000, Some((0x0000_0008, 0xffff_ffff)))
            .expect("an implemented BAR");

        assert_eq!(bar.base(), 0x8_0000_0000);
        assert_eq!(bar.bytes(), 0x40_0000);
        assert_eq!(bar.kind(), BarKind::Memory { prefetchable: true, wide: true });
    }

    #[test]
    fn unimplemented_bar_reports_nothing() {
        assert_eq!(decode(0, 0, 0, None), None);
        assert_eq!(decode(0, 0, 0, Some((0, 0))), None);
    }

    #[test]
    fn bar32_ignores_next_register() {
        // The upper half of a 32-bit BAR is not a register, so the mask above
        // bit 31 is not writable and must not lengthen the window.
        let bar = decode(0, 0xfebc_0000, 0xffff_fff0, None).expect("an implemented BAR");

        assert_eq!(bar.bytes(), 0x10);
    }

    #[test]
    fn io_bar_keeps_reserved_bits() {
        let bar = decode(1, 0xc001, 0xffff_fffd, None).expect("an implemented BAR");

        assert_eq!(bar.base(), 0xc000);
        assert_eq!(bar.kind(), BarKind::Io);
        assert_eq!(bar.span().err(), Some(molt_arch::memory::Error::Kind));
    }

    #[test]
    fn span_rounds_to_whole_frames() {
        let bar = Bar {
            index: 0,
            base: 0xfebc_1000,
            len: 0x100,
            kind: BarKind::Memory { prefetchable: false, wide: false },
        };

        let span = bar.span().expect("a frame-aligned window");

        assert_eq!(span.start(), 0xfebc_1000);
        assert_eq!(span.end(), 0xfebc_2000);
    }

    #[test]
    fn sizing_restores_register_and_command() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001).register(0x10, 0xfebc_0000);
        let mut function = Function::probe(space.config(0, 0), Address::new(0, 0, 0).unwrap())
            .expect("a legal read")
            .expect("present");
        let wanted = Command::MEMORY.with(Command::BUS_MASTER);
        function.set_command(wanted).expect("a legal write");

        function.bar(0).expect("a legal probe");

        assert_eq!(function.command(), Ok(wanted), "decode was left disabled");
        assert_eq!(function.window().read_u32(0x10), Ok(0xfebc_0000));
    }

    #[test]
    fn out_of_range_bar_index_refused() {
        let mut space = Space::new();
        space.function(0, 0).header(0x1234, 0x0001);
        let mut function = Function::probe(space.config(0, 0), Address::new(0, 0, 0).unwrap())
            .expect("a legal read")
            .expect("present");

        assert_eq!(function.bar(6), Err(PciError::Layout));
    }
}
