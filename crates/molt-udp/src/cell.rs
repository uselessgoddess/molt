//! The restart boundary around a UDP service.

use molt_core::cell::Cell;
use molt_net::address::Ipv4Address;

use crate::{Scratch, Udp, UdpError};

/// The addressing and registered buffers retained across a restart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpState {
    local: Ipv4Address,
    tx: Scratch,
    rx: Scratch,
}

impl UdpState {
    /// Retains only static addressing and service-owned scratch buffers.
    pub const fn new(local: Ipv4Address, tx: Scratch, rx: Scratch) -> Self {
        Self { local, tx, rx }
    }
}

/// A restartable UDP service with no sockets outside its current epoch.
pub struct UdpCell<const N: usize, const Q: usize> {
    udp: Udp<N, Q>,
}

impl<const N: usize, const Q: usize> UdpCell<N, Q> {
    /// Borrows the current service epoch.
    pub fn udp(&mut self) -> &mut Udp<N, Q> {
        &mut self.udp
    }
}

impl<const N: usize, const Q: usize> Cell for UdpCell<N, Q> {
    type State = UdpState;
    type Error = UdpError;

    fn spawn(state: Self::State) -> Result<Self, Self::Error> {
        Ok(Self { udp: Udp::new(state.local, state.tx, state.rx) })
    }

    fn restart(&mut self) -> Result<(), Self::Error> {
        self.udp.reset();
        Ok(())
    }
}
