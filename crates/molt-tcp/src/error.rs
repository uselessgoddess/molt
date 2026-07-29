//! TCP service errors.

use molt_core::buffer::BufferError;
use molt_core::capability::CapabilityError;

/// Why a TCP operation could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpError {
    Capability(CapabilityError),
    RegisteredBuffer(BufferError),
    /// The peer reset the connection, or it never came up.
    Reset,
    /// The socket already has a request parked on it.
    Busy,
    /// No free socket, or no room to park another request.
    Full,
    /// The endpoint is unusable: a zero port, or an unspecified address.
    Unaddressable,
}

impl From<CapabilityError> for TcpError {
    fn from(error: CapabilityError) -> Self {
        Self::Capability(error)
    }
}

impl From<BufferError> for TcpError {
    fn from(error: BufferError) -> Self {
        Self::RegisteredBuffer(error)
    }
}
