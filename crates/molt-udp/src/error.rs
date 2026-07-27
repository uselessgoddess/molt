//! UDP service and wire errors.

use molt_core::buffer::BufferError;
use molt_core::capability::CapabilityError;
use molt_net::{IpError, NetError};

/// Why a UDP operation could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UdpError {
    Buffer,
    Malformed,
    Checksum,
    Wire(NetError),
    Ip(IpError),
    Capability(CapabilityError),
    RegisteredBuffer(BufferError),
    Bound,
    Busy,
    Full,
    TooLarge,
}

impl From<NetError> for UdpError {
    fn from(error: NetError) -> Self {
        Self::Wire(error)
    }
}

impl From<IpError> for UdpError {
    fn from(error: IpError) -> Self {
        Self::Ip(error)
    }
}

impl From<CapabilityError> for UdpError {
    fn from(error: CapabilityError) -> Self {
        Self::Capability(error)
    }
}

impl From<BufferError> for UdpError {
    fn from(error: BufferError) -> Self {
        Self::RegisteredBuffer(error)
    }
}
