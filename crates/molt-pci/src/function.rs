//! A typed view of one function's configuration space.

use core::ops::BitOr;

use crate::address::Address;
use crate::bar::{Bar, Bars};
use crate::capability::Capabilities;
use crate::config::{self, Config};
use crate::error::Error;
use crate::msix::MsiX;

/// Who made the device and what it is, as the vendor assigned it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Id {
    pub vendor: u16,
    pub device: u16,
}

/// What the device does, as the specification classifies it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Class {
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
}

/// Which register layout the rest of the header follows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layout {
    /// A leaf device: six base address registers.
    Endpoint,
    /// A bridge: two base address registers and a secondary bus behind it.
    Bridge,
    Other(u8),
}

impl Layout {
    const fn of(header: u8) -> Self {
        match header & 0x7f {
            0x00 => Self::Endpoint,
            0x01 => Self::Bridge,
            other => Self::Other(other),
        }
    }

    /// How many base address registers this layout defines.
    pub const fn bars(self) -> u8 {
        match self {
            Self::Endpoint => 6,
            Self::Bridge => 2,
            Self::Other(_) => 0,
        }
    }
}

/// The command-register bits this kernel turns on and off.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Command(u16);

impl Command {
    /// Decode I/O port space. Never set here; it exists to be cleared.
    pub const IO: Self = Self(1 << 0);
    /// Decode memory space. Off while a BAR is being measured.
    pub const MEMORY: Self = Self(1 << 1);
    /// Issue DMA. Off until the device's frames are owned.
    pub const BUS_MASTER: Self = Self(1 << 2);
    /// Suppress the legacy wired interrupt, which MSI-X replaces rather than
    /// shares: a device left able to raise both can deliver an edge through a
    /// path nothing is waiting on.
    pub const INTX_DISABLE: Self = Self(1 << 10);

    pub const fn contains(self, bits: Self) -> bool {
        self.0 & bits.0 == bits.0
    }

    pub const fn with(self, bits: Self) -> Self {
        Self(self.0 | bits.0)
    }

    pub const fn without(self, bits: Self) -> Self {
        Self(self.0 & !bits.0)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }
}

impl BitOr for Command {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.with(other)
    }
}

/// One function, reached through the configuration window it lives in.
pub struct Function<'c, C: Config + ?Sized> {
    config: &'c C,
    at: Address,
}

// A shared reference is copyable whatever it points at, but a derive would
// demand `C: Copy` and no transport is.
impl<C: Config + ?Sized> Clone for Function<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C: Config + ?Sized> Copy for Function<'_, C> {}

impl<'c, C: Config + ?Sized> Function<'c, C> {
    /// Reads the function at `at`, or reports [`Error::Absent`] where the bus
    /// answers for nobody.
    pub fn probe(config: &'c C, at: Address) -> Result<Self, Error> {
        let function = Self { config, at };
        if function.id().vendor == config::ABSENT {
            return Err(Error::Absent);
        }
        Ok(function)
    }

    pub fn address(self) -> Address {
        self.at
    }

    pub fn config(self) -> &'c C {
        self.config
    }

    pub fn id(self) -> Id {
        let word = self.config.read(self.at, config::ID);
        Id { vendor: word as u16, device: (word >> 16) as u16 }
    }

    pub fn class(self) -> Class {
        let word = self.config.read(self.at, config::CLASS);
        Class {
            interface: (word >> 8) as u8,
            subclass: (word >> 16) as u8,
            class: (word >> 24) as u8,
        }
    }

    pub fn layout(self) -> Layout {
        Layout::of((self.config.read(self.at, config::HEADER) >> 16) as u8)
    }

    /// Whether the device implements functions past zero. Only function zero
    /// answers this, so a sweep asks it there and nowhere else.
    pub fn multifunction(self) -> bool {
        self.config.read(self.at, config::HEADER) & config::MULTIFUNCTION != 0
    }

    pub fn command(self) -> Command {
        Command(self.config.read(self.at, config::COMMAND) as u16)
    }

    pub fn set_command(self, command: Command) {
        config::write16(self.config, self.at, config::COMMAND, command.bits());
    }

    pub fn enable(self, bits: Command) {
        self.set_command(self.command().with(bits));
    }

    pub fn disable(self, bits: Command) {
        self.set_command(self.command().without(bits));
    }

    /// Measures one base address register.
    pub fn bar(self, index: u8) -> Result<Bar, Error> {
        Bar::measure(self, index)
    }

    /// Every implemented memory window, in register order.
    pub fn bars(self) -> Bars<'c, C> {
        Bars::new(self)
    }

    /// The function's capability list, empty where it reports none.
    pub fn capabilities(self) -> Capabilities<'c, C> {
        Capabilities::new(self)
    }

    /// The MSI-X capability, or [`Error::Missing`] on a device that has none.
    pub fn msix(self) -> Result<MsiX<'c, C>, Error> {
        MsiX::of(self)
    }
}
