//! Base address registers, and what it costs to learn their size.

use crate::config::{self, Config};
use crate::error::Error;
use crate::function::{Command, Function};

/// Low bit of a base address register: set means I/O space.
const IO: u32 = 1 << 0;
/// Type field: `0` is a 32-bit window, `2` is the low half of a 64-bit one.
const TYPE: u32 = 0b110;
const TYPE_64: u32 = 0b100;
/// Set where reads have no side effects and the window may be prefetched.
const PREFETCHABLE: u32 = 1 << 3;
/// The low four bits describe the register; they are not part of the address.
const ADDRESS: u32 = !0xf;

/// How many address bits the window is programmed with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Width {
    Bits32,
    Bits64,
}

/// One decoded memory window of a function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bar {
    index: u8,
    base: u64,
    size: u64,
    width: Width,
    prefetchable: bool,
}

impl Bar {
    /// Which base address register this came from. A 64-bit window occupies
    /// this register and the next one.
    pub const fn index(self) -> u8 {
        self.index
    }

    /// The physical address the window decodes at, as firmware programmed it.
    pub const fn base(self) -> u64 {
        self.base
    }

    /// The window's length in bytes, always a power of two.
    pub const fn size(self) -> u64 {
        self.size
    }

    pub const fn width(self) -> Width {
        self.width
    }

    pub const fn prefetchable(self) -> bool {
        self.prefetchable
    }

    /// The register the next window would live in.
    pub const fn next(self) -> u8 {
        match self.width {
            Width::Bits32 => self.index + 1,
            Width::Bits64 => self.index + 2,
        }
    }

    /// Measures the register by writing all ones and reading back which
    /// address bits the device implements.
    ///
    /// Sizing is a write, and a half-written register decodes somewhere the
    /// caller did not choose, so memory and I/O decoding are turned off for the
    /// duration and restored to exactly what they were. This is why it is a
    /// boot-time operation on a device nothing is using yet.
    pub(crate) fn measure<C: Config + ?Sized>(
        function: Function<'_, C>,
        index: u8,
    ) -> Result<Self, Error> {
        if index >= function.layout().bars() {
            return Err(Error::Bar);
        }
        let low = register(function, index);
        if low & IO != 0 {
            return Err(Error::NotMemory);
        }
        let width = match low & TYPE {
            0 => Width::Bits32,
            TYPE_64 => Width::Bits64,
            // The one remaining encoding is a 1 MiB window below the first
            // megabyte, deleted from the specification decades ago.
            _ => return Err(Error::NotMemory),
        };
        if width == Width::Bits64 && index + 1 >= function.layout().bars() {
            return Err(Error::Bar);
        }
        let high = match width {
            Width::Bits32 => 0,
            Width::Bits64 => register(function, index + 1),
        };

        let command = function.command();
        function.disable(Command::MEMORY | Command::IO);
        let mask = probe(function, index, low, width, high);
        function.set_command(command);

        let mask = mask & !0xf;
        if mask == 0 {
            return Err(Error::Bar);
        }
        // The lowest address bit the device implements *is* the size: a window
        // decodes on `size` alignment, so every bit below it is hardwired low.
        // Reading it this way needs no assumption about how many upper bits a
        // 64-bit register left writable.
        let size = 1 << mask.trailing_zeros();
        let base = u64::from(low & ADDRESS) | u64::from(high) << 32;
        Ok(Self { index, base, size, width, prefetchable: low & PREFETCHABLE != 0 })
    }
}

/// Writes all ones, reads the implemented address bits back, and restores the
/// register to what the caller found there.
fn probe<C: Config + ?Sized>(
    function: Function<'_, C>,
    index: u8,
    low: u32,
    width: Width,
    high: u32,
) -> u64 {
    write(function, index, !0);
    let mask = u64::from(register(function, index));
    write(function, index, low);
    match width {
        Width::Bits32 => mask,
        Width::Bits64 => {
            write(function, index + 1, !0);
            let upper = u64::from(register(function, index + 1)) << 32;
            write(function, index + 1, high);
            mask | upper
        }
    }
}

fn register<C: Config + ?Sized>(function: Function<'_, C>, index: u8) -> u32 {
    function.config().read(function.address(), config::BAR + u16::from(index) * 4)
}

fn write<C: Config + ?Sized>(function: Function<'_, C>, index: u8, value: u32) {
    function.config().write(function.address(), config::BAR + u16::from(index) * 4, value);
}

/// Every implemented memory window of a function, skipping the halves of a
/// 64-bit register and the registers the device left unimplemented.
pub struct Bars<'c, C: Config + ?Sized> {
    function: Function<'c, C>,
    index: u8,
}

impl<'c, C: Config + ?Sized> Bars<'c, C> {
    pub(crate) fn new(function: Function<'c, C>) -> Self {
        Self { function, index: 0 }
    }
}

impl<C: Config + ?Sized> Iterator for Bars<'_, C> {
    type Item = Bar;

    fn next(&mut self) -> Option<Bar> {
        while self.index < self.function.layout().bars() {
            let index = self.index;
            match Bar::measure(self.function, index) {
                Ok(bar) => {
                    self.index = bar.next();
                    return Some(bar);
                }
                // An unimplemented or I/O register is one word wide; only a
                // measured 64-bit window consumes two.
                Err(_) => self.index = index + stride(register(self.function, index)),
            }
        }
        None
    }
}

const fn stride(low: u32) -> u8 {
    if low & IO == 0 && low & TYPE == TYPE_64 { 2 } else { 1 }
}
