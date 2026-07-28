//! The link presented to smoltcp as a device.

use molt_net::Link;
use smoltcp::phy::{Device as PhyDevice, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

/// The largest Ethernet frame a link carries.
const FRAME: usize = 1514;

/// A link with one frame of scratch each way.
pub struct Device<L> {
    link: L,
    rx: [u8; FRAME],
    tx: [u8; FRAME],
}

impl<L: Link> Device<L> {
    pub const fn new(link: L) -> Self {
        Self { link, rx: [0; FRAME], tx: [0; FRAME] }
    }

    pub const fn link(&self) -> &L {
        &self.link
    }

    pub fn into_link(self) -> L {
        self.link
    }
}

impl<L: Link> PhyDevice for Device<L> {
    type RxToken<'a>
        = Rx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = Tx<'a, L>
    where
        Self: 'a;

    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, Tx<'_, L>)> {
        let Self { link, rx, tx } = self;
        let len = link.receive(rx).ok().flatten()?;
        Some((Rx(&rx[..len]), Tx { link, frame: tx }))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_, L>> {
        let Self { link, tx, .. } = self;
        Some(Tx { link, frame: tx })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = FRAME;
        capabilities
    }
}

/// One frame the link had waiting.
pub struct Rx<'a>(&'a [u8]);

impl RxToken for Rx<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.0)
    }
}

/// Room for one frame, transmitted once it is written.
pub struct Tx<'a, L> {
    link: &'a mut L,
    frame: &'a mut [u8],
}

impl<L: Link> TxToken for Tx<'_, L> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let result = f(&mut self.frame[..len]);
        // A drop here is a lost segment, which is what retransmission is for.
        let _ = self.link.transmit(&self.frame[..len]);
        result
    }
}
