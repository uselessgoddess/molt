//! Interrupt identities, and the message a device writes to raise one.
//!
//! A message-signalled interrupt is a memory write: the device stores
//! [`MsiMessage::data`] at [`MsiMessage::address`] and the interrupt fabric
//! turns that store into an interrupt on some CPU. Both halves are platform
//! detail — x86_64 encodes a vector and a destination APIC ID, RISC-V's IMSIC
//! encodes an identity and a hart's interrupt file — so PCI never computes
//! either. It asks the platform for a message and writes what it is given.
//!
//! [`Sink`] is the other direction: the platform's interrupt entry path calls
//! it with the line that fired. The line is a bare `u16` rather than a
//! `molt-core` type because `molt-arch` is the layer below `molt-core`; the
//! kernel supplies the adapter that turns a line into a woken task, the same
//! way [`Owner::Device`](crate::memory::Owner::Device) carries an opaque `u32`.

/// The store a device performs to raise an interrupt.
///
/// Produced by [`InterruptFabric::allocate`] and never constructed by a driver:
/// getting either half wrong silently delivers to the wrong CPU, or nowhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsiMessage {
    address: u64,
    data: u32,
}

impl MsiMessage {
    pub const fn new(address: u64, data: u32) -> Self {
        Self { address, data }
    }

    pub const fn address(self) -> u64 {
        self.address
    }

    pub const fn data(self) -> u32 {
        self.data
    }
}

/// Why an interrupt identity could not be handed out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FabricError {
    /// The platform has no message-signalled interrupt fabric.
    ///
    /// Returned rather than silently falling back to a pin-based interrupt: a
    /// driver that cannot get a vector should say so, not run with a route it
    /// did not ask for.
    Unsupported,
    /// Every identity the platform reserved for devices is already in use.
    Exhausted,
    /// The line does not name an identity this fabric issued.
    Unknown,
}

/// Hands out interrupt identities and describes how a device raises them.
pub trait InterruptFabric {
    /// Reserves one identity and returns the line it will arrive on, together
    /// with the message a device must be programmed with to raise it.
    fn allocate(&mut self) -> Result<(u16, MsiMessage), FabricError>;

    /// Returns an identity to the fabric.
    ///
    /// The caller is responsible for having stopped the device first. A device
    /// left programmed with a released message keeps writing it, which is why
    /// [`Sink::raise`] must stay safe to call for a line nobody is waiting on.
    fn release(&mut self, line: u16) -> Result<(), FabricError>;
}

/// Where a raised interrupt line goes.
///
/// Called from interrupt context, so an implementation must be wait-free and
/// must not allocate, lock, or panic.
pub trait Sink: Sync {
    /// Records that `line` fired.
    ///
    /// Spurious calls are permitted and must be harmless: a device that was
    /// reprogrammed while a write was in flight can raise a line whose owner
    /// has already gone away.
    fn raise(&self, line: u16);
}
