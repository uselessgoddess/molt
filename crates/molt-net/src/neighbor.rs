//! Protocol-neutral neighbor discovery cache.

use crate::address::{IpAddr, MacAddress};

#[derive(Clone, Copy)]
struct Neighbor {
    address: IpAddr,
    hardware: MacAddress,
}

/// Fixed-capacity IP-to-link-address cache shared by discovery protocols.
pub struct Cache<const N: usize> {
    entries: [Option<Neighbor>; N],
}

impl<const N: usize> Cache<N> {
    pub const fn new() -> Self {
        Self { entries: [None; N] }
    }

    pub fn resolve(&self, address: IpAddr) -> Option<MacAddress> {
        self.entries
            .iter()
            .flatten()
            .find(|neighbor| neighbor.address == address)
            .map(|neighbor| neighbor.hardware)
    }

    pub fn learn(&mut self, address: IpAddr, hardware: MacAddress) {
        if let Some(neighbor) =
            self.entries.iter_mut().flatten().find(|neighbor| neighbor.address == address)
        {
            neighbor.hardware = hardware;
            return;
        }
        if let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(Neighbor { address, hardware });
        } else if let Some(slot) = self.entries.first_mut() {
            *slot = Some(Neighbor { address, hardware });
        }
    }
}

impl<const N: usize> Default for Cache<N> {
    fn default() -> Self {
        Self::new()
    }
}
