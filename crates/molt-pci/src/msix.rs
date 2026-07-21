//! MSI-X: the capability that says where the table is, and the table itself.

use crate::capability;
use crate::config::{self, Config};
use crate::error::Error;
use crate::function::Function;
use crate::message::Message;

/// Control half-word, two bytes into the capability.
const CONTROL: u16 = 2;
/// Table location: BIR in the low three bits, offset in the rest.
const TABLE: u16 = 4;
/// Pending-bit array location, encoded the same way.
const PENDING: u16 = 8;

/// Deliver messages at all.
const ENABLE: u16 = 1 << 15;
/// Mask every vector regardless of its own mask bit.
const FUNCTION_MASK: u16 = 1 << 14;
/// One less than the number of table entries.
const SIZE: u16 = 0x7ff;

/// Bytes one table entry occupies.
pub const ENTRY: u64 = 16;

const ADDRESS_LOW: usize = 0;
const ADDRESS_HIGH: usize = 1;
const DATA: usize = 2;
const VECTOR_CONTROL: usize = 3;
/// Vector control bit zero: set means this entry delivers nothing.
const MASKED: u32 = 1 << 0;

/// Where in a BAR the device put one of its MSI-X structures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Location {
    bar: u8,
    offset: u32,
}

impl Location {
    /// Which base address register the structure lives in.
    pub const fn bar(self) -> u8 {
        self.bar
    }

    /// Byte offset from the start of that window.
    pub const fn offset(self) -> u32 {
        self.offset
    }
}

/// The MSI-X capability of one function.
///
/// This is the configuration-space half: how many vectors there are, where the
/// table is, and whether delivery is on. The table itself is memory in a BAR,
/// so it is reached through [`Table`] once the platform has mapped that BAR.
pub struct MsiX<'c, C: Config + ?Sized> {
    function: Function<'c, C>,
    offset: u16,
}

// As for `Function`: copyable, but not by a derive that would want `C: Copy`.
impl<C: Config + ?Sized> Clone for MsiX<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C: Config + ?Sized> Copy for MsiX<'_, C> {}

impl<'c, C: Config + ?Sized> MsiX<'c, C> {
    pub(crate) fn of(function: Function<'c, C>) -> Result<Self, Error> {
        let capability = function
            .capabilities()
            .find(|capability| capability.id() == capability::MSIX)
            .ok_or(Error::Missing)?;
        Ok(Self { function, offset: capability.offset() })
    }

    /// How many vectors the device implements, at least one.
    pub fn vectors(self) -> u16 {
        (self.control() & SIZE) + 1
    }

    pub fn table(self) -> Location {
        self.location(TABLE)
    }

    pub fn pending(self) -> Location {
        self.location(PENDING)
    }

    pub fn enabled(self) -> bool {
        self.control() & ENABLE != 0
    }

    /// Turns delivery on with every vector still individually masked, which is
    /// the only order that is safe: the function mask is one bit, whereas the
    /// table entries hold whatever was in that BAR before the kernel wrote it.
    pub fn enable(self) {
        self.set_control(self.control() | ENABLE | FUNCTION_MASK);
    }

    pub fn disable(self) {
        self.set_control(self.control() & !ENABLE);
    }

    /// Masks or unmasks every vector at once, above their own mask bits.
    pub fn mask_all(self, masked: bool) {
        let control = self.control();
        self.set_control(match masked {
            true => control | FUNCTION_MASK,
            false => control & !FUNCTION_MASK,
        });
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

    fn location(self, register: u16) -> Location {
        let word = self.function.config().read(self.function.address(), self.offset + register);
        Location { bar: (word & 0b111) as u8, offset: word & !0b111 }
    }
}

/// A mapped MSI-X table.
///
/// Entries are device memory, so every access is volatile and the writes are
/// ordered by hand: an entry is masked before its address and data change and
/// unmasked afterwards, because a device that raises the vector mid-update
/// would otherwise post a write to half of one address and half of another.
pub struct Table {
    base: *mut u32,
    vectors: u16,
}

impl Table {
    /// # Safety
    ///
    /// `base` must be a live, exclusively owned mapping of at least `vectors`
    /// entries of an MSI-X table, mapped as device memory. Nothing else may
    /// write the table for as long as this value exists.
    pub const unsafe fn new(base: *mut u32, vectors: u16) -> Self {
        Self { base, vectors }
    }

    pub const fn vectors(&self) -> u16 {
        self.vectors
    }

    /// Points one vector at `message` and leaves it masked.
    pub fn program(&self, index: u16, message: Message) -> Result<(), Error> {
        let entry = self.entry(index)?;
        // SAFETY: `entry` is inside the table `new`'s caller vouched for, and
        // the mask is written first so the device cannot deliver a vector built
        // from a mixture of the old address and the new one.
        unsafe {
            entry.add(VECTOR_CONTROL).write_volatile(MASKED);
            entry.add(ADDRESS_LOW).write_volatile(message.address() as u32);
            entry.add(ADDRESS_HIGH).write_volatile((message.address() >> 32) as u32);
            entry.add(DATA).write_volatile(message.data());
        }
        Ok(())
    }

    pub fn mask(&self, index: u16, masked: bool) -> Result<(), Error> {
        let entry = self.entry(index)?;
        // SAFETY: as in `program`; this writes only the entry's own control.
        unsafe {
            entry.add(VECTOR_CONTROL).write_volatile(match masked {
                true => MASKED,
                false => 0,
            });
        }
        Ok(())
    }

    pub fn masked(&self, index: u16) -> Result<bool, Error> {
        let entry = self.entry(index)?;
        // SAFETY: as in `program`; this reads only the entry's own control.
        Ok(unsafe { entry.add(VECTOR_CONTROL).read_volatile() } & MASKED != 0)
    }

    fn entry(&self, index: u16) -> Result<*mut u32, Error> {
        if index >= self.vectors {
            return Err(Error::Vector);
        }
        // SAFETY: the offset stays inside the mapping, since `index` is below
        // the vector count `new`'s caller mapped for.
        Ok(unsafe { self.base.byte_add(usize::from(index) * ENTRY as usize) })
    }
}
