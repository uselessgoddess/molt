//! The restart boundary around a TCP service.

use molt_core::cell::Cell;
use molt_net::{Config, Link};
use smoltcp::iface::SocketStorage;

use crate::{Tcp, TcpError};

/// The link and borrowed storage a TCP service starts from.
pub struct TcpState<'a, L> {
    link: L,
    config: Config,
    slots: &'a mut [SocketStorage<'a>],
    rings: &'a mut [u8],
}

impl<'a, L> TcpState<'a, L> {
    pub const fn new(
        link: L,
        config: Config,
        slots: &'a mut [SocketStorage<'a>],
        rings: &'a mut [u8],
    ) -> Self {
        Self { link, config, slots, rings }
    }
}

/// A restartable TCP service with no stream outside its current epoch.
pub struct TcpCell<'a, L, const N: usize> {
    tcp: Tcp<'a, L, N>,
}

impl<'a, L: Link, const N: usize> TcpCell<'a, L, N> {
    /// Borrows the current service epoch.
    pub fn tcp(&mut self) -> &mut Tcp<'a, L, N> {
        &mut self.tcp
    }
}

impl<'a, L: Link, const N: usize> Cell for TcpCell<'a, L, N> {
    type State = TcpState<'a, L>;
    type Error = TcpError;

    fn spawn(state: Self::State) -> Result<Self, Self::Error> {
        Ok(Self { tcp: Tcp::new(state.link, state.config, state.slots, state.rings)? })
    }

    fn restart(&mut self) -> Result<(), Self::Error> {
        self.tcp.reset();
        Ok(())
    }
}
