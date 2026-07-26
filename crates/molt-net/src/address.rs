//! Link and IPv4 addresses.

/// A six-byte Ethernet address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    pub const BROADCAST: Self = Self([u8::MAX; 6]);

    pub const fn new(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

/// A four-byte IPv4 address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Ipv4Address([u8; 4]);

impl Ipv4Address {
    pub const UNSPECIFIED: Self = Self([0; 4]);

    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    pub const fn from_octets(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    pub const fn octets(self) -> [u8; 4] {
        self.0
    }
}
