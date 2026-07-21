//! MSI: the capability that holds its one message in configuration space.
//!
//! MSI-X came later and is better in every way that matters here — a table per
//! device, an address and a data word per vector, a mask bit per vector — but
//! the older form is what a great many devices implement, and it is small
//! enough that supporting it costs one register write more than refusing to.
//!
//! The two differences that shape this module: the message lives in the
//! capability itself rather than in a BAR, so programming it needs no mapping
//! at all; and a device that asks for several vectors is given a *block* of
//! consecutive ones whose low bits it sets itself, so the kernel hands out one
//! vector and tells the device to use exactly one.

use crate::capability;
use crate::config::{self, Config};
use crate::error::Error;
use crate::function::Function;
use crate::message::Message;

/// Message control, two bytes into the capability.
const CONTROL: u16 = 2;
/// The low half of the message address, four bytes in.
const ADDRESS: u16 = 4;

/// Deliver messages at all.
const ENABLE: u16 = 1 << 0;
/// How many vectors the device can use, as a power of two exponent.
const CAPABLE: u16 = 0b111 << 1;
/// How many it may use, encoded the same way. Zero means one vector.
const ENABLED: u16 = 0b111 << 4;
/// Set where the message address is 64 bits wide, which moves the data word.
const WIDE: u16 = 1 << 7;

/// The MSI capability of one function.
///
/// Programming order is the whole of the care needed here: the message is three
/// registers, a device with delivery enabled may raise an interrupt between any
/// two of them, and the vector it would raise is built from whichever halves
/// have been written. So [`program`](Self::program) disables delivery first and
/// leaves it disabled; [`enable`](Self::enable) is a separate call the caller
/// makes once the message is whole.
pub struct Msi<'c, C: Config + ?Sized> {
    function: Function<'c, C>,
    offset: u16,
}

// As for `Function`: copyable, but not by a derive that would want `C: Copy`.
impl<C: Config + ?Sized> Clone for Msi<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C: Config + ?Sized> Copy for Msi<'_, C> {}

impl<'c, C: Config + ?Sized> Msi<'c, C> {
    pub(crate) fn of(function: Function<'c, C>) -> Result<Self, Error> {
        let capability = function
            .capabilities()
            .find(|capability| capability.id() == capability::MSI)
            .ok_or(Error::Missing)?;
        Ok(Self { function, offset: capability.offset() })
    }

    /// How many vectors the device could use if it were given a block, at least
    /// one. The kernel gives it one regardless; this is what it asked for.
    pub fn vectors(self) -> u16 {
        1 << ((self.control() & CAPABLE) >> 1)
    }

    /// Whether the message address is 64 bits, which decides where the data
    /// word lives and whether the device can be pointed at a high address.
    pub fn wide(self) -> bool {
        self.control() & WIDE != 0
    }

    pub fn enabled(self) -> bool {
        self.control() & ENABLE != 0
    }

    /// Points the device's one message at `message`, with delivery off.
    ///
    /// The vector count is written down to one at the same time: a device left
    /// enabled for a block would drive the low bits of the data word itself and
    /// raise vectors the kernel never allocated.
    pub fn program(self, message: Message) -> Result<(), Error> {
        self.disable();
        let control = self.control();
        if !self.fits(message, control) {
            return Err(Error::Address);
        }
        self.write(ADDRESS, message.address() as u32);
        if control & WIDE != 0 {
            self.write(ADDRESS + 4, (message.address() >> 32) as u32);
        }
        config::write16(
            self.function.config(),
            self.function.address(),
            self.offset + self.data(control),
            message.data() as u16,
        );
        self.set_control(control & !ENABLED);
        Ok(())
    }

    pub fn enable(self) {
        self.set_control(self.control() | ENABLE);
    }

    pub fn disable(self) {
        self.set_control(self.control() & !ENABLE);
    }

    /// Whether the device can be pointed at this message at all.
    ///
    /// A narrow device cannot hold an address above four gigabytes, and no
    /// device holds more than sixteen bits of data. Refusing is the only honest
    /// answer: a truncated address is a posted write to a frame the kernel
    /// chose by accident.
    fn fits(self, message: Message, control: u16) -> bool {
        (control & WIDE != 0 || message.address() <= u32::MAX.into())
            && message.data() <= u16::MAX.into()
    }

    /// Where the data half-word sits, which the address width moves.
    const fn data(self, control: u16) -> u16 {
        match control & WIDE {
            0 => 8,
            _ => 12,
        }
    }

    fn control(self) -> u16 {
        config::read16(self.function.config(), self.function.address(), self.offset + CONTROL)
    }

    fn set_control(self, value: u16) {
        config::write16(
            self.function.config(),
            self.function.address(),
            self.offset + CONTROL,
            value,
        );
    }

    fn write(self, register: u16, value: u32) {
        self.function.config().write(self.function.address(), self.offset + register, value);
    }
}
