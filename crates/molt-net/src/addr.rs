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

    /// The Ethernet address an IPv6 multicast group is delivered to (RFC 2464).
    pub const fn multicast(group: Ipv6Addr) -> Self {
        let octets = group.octets();
        Self([0x33, 0x33, octets[12], octets[13], octets[14], octets[15]])
    }

    /// Whether the address is a group rather than one station.
    pub const fn is_multicast(self) -> bool {
        self.0[0] & 1 != 0
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

/// The address a host answers discovery for `addr` on (RFC 4291 §2.7.1).
///
/// Solicitations reach it instead of the all-nodes group, so a link only wakes
/// the hosts whose low twenty-four bits collide.
pub const fn solicited_node(addr: Ipv6Addr) -> Ipv6Addr {
    let octets = addr.octets();
    let low = u16::from_be_bytes([octets[14], octets[15]]);
    Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00 | octets[13] as u16, low)
}

/// The link-local address a MAC gives a host, by the modified EUI-64 rule.
pub const fn link_local(mac: MacAddr) -> Ipv6Addr {
    let octets = mac.octets();
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([octets[0] ^ 0x02, octets[1]]),
        u16::from_be_bytes([octets[2], 0xff]),
        u16::from_be_bytes([0xfe, octets[3]]),
        u16::from_be_bytes([octets[4], octets[5]]),
    )
}
