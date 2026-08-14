//! The frame boundary below the IP service.

/// Why a link did not accept a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkError {
    Busy,
    Device,
}

/// A device that carries complete Ethernet frames both ways.
pub trait Link {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError>;

    /// Takes the next frame the device has for the host, if any.
    fn receive(&mut self, frame: &mut [u8]) -> Result<Option<usize>, LinkError>;
}
