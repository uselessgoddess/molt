//! The frame boundary below the IP service.

/// Why a link did not accept a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkError {
    Busy,
    Device,
}

/// A device that accepts complete Ethernet frames.
pub trait Link {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError>;
}
