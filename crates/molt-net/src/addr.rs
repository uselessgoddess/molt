//! Link and IP addresses.

pub use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A six-byte Ethernet address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    pub const BROADCAST: Self = Self([u8::MAX; 6]);

    pub const fn new(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}
