//! Interrupt identities and the message a device writes to raise one.
//!
//! An MSI is a memory write: the device stores [`MsiMessage::data`] at
//! [`MsiMessage::address`], and the platform's interrupt fabric decodes that
//! store into a vector. Both halves are platform detail, so PCI never computes
//! them — it asks the fabric and writes what it is given.
//!
//! [`Sink`] is the reverse direction: the interrupt entry path calls it with
//! the line that fired. The line is a bare `u16` because `molt-arch` sits
//! below `molt-core`; the kernel supplies the adapter.

/// The store a device performs to raise an interrupt.
///
/// Produced by [`InterruptFabric::allocate`]; never constructed by a driver.
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
    /// No message-signalled interrupt fabric on this platform.
    Unsupported,
    /// Every reserved identity is already in use.
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
    /// Records that `line` fired. Spurious calls must be harmless.
    fn raise(&self, line: u16);
}
