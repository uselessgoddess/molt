//! Ethernet II frame encoding.

use crate::NetError;
use crate::address::MacAddress;

const HEADER: usize = 14;

/// A supported Ethernet protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EtherType {
    Ipv4,
    Ipv6,
    Arp,
}

impl EtherType {
    const fn value(self) -> u16 {
        match self {
            Self::Ipv4 => 0x0800,
            Self::Ipv6 => 0x86dd,
            Self::Arp => 0x0806,
        }
    }

    fn parse(value: u16) -> Result<Self, NetError> {
        match value {
            0x0800 => Ok(Self::Ipv4),
            0x86dd => Ok(Self::Ipv6),
            0x0806 => Ok(Self::Arp),
            _ => Err(NetError::Unsupported),
        }
    }
}

/// An Ethernet header and borrowed payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame<'a> {
    destination: MacAddress,
    source: MacAddress,
    ether_type: EtherType,
    payload: &'a [u8],
}

impl<'a> Frame<'a> {
    pub const fn new(
        destination: MacAddress,
        source: MacAddress,
        ether_type: EtherType,
        payload: &'a [u8],
    ) -> Self {
        Self { destination, source, ether_type, payload }
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self, NetError> {
        if bytes.len() < HEADER {
            return Err(NetError::Malformed);
        }
        let destination = MacAddress::new(bytes[0..6].try_into().unwrap());
        let source = MacAddress::new(bytes[6..12].try_into().unwrap());
        let ether_type = EtherType::parse(u16::from_be_bytes(bytes[12..14].try_into().unwrap()))?;
        Ok(Self::new(destination, source, ether_type, &bytes[HEADER..]))
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, NetError> {
        let len = HEADER.checked_add(self.payload.len()).ok_or(NetError::Buffer)?;
        if bytes.len() < len {
            return Err(NetError::Buffer);
        }
        bytes[0..6].copy_from_slice(&self.destination.octets());
        bytes[6..12].copy_from_slice(&self.source.octets());
        bytes[12..14].copy_from_slice(&self.ether_type.value().to_be_bytes());
        bytes[HEADER..len].copy_from_slice(self.payload);
        Ok(len)
    }

    pub const fn destination(&self) -> MacAddress {
        self.destination
    }

    pub const fn source(&self) -> MacAddress {
        self.source
    }

    pub const fn ether_type(&self) -> EtherType {
        self.ether_type
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}
