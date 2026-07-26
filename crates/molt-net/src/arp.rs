//! Ethernet/IPv4 address resolution packets.

use crate::NetError;
use crate::address::{Ipv4Address, MacAddress};

const LEN: usize = 28;

/// An ARP request or reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Request,
    Reply,
}

impl Operation {
    const fn value(self) -> u16 {
        match self {
            Self::Request => 1,
            Self::Reply => 2,
        }
    }

    fn parse(value: u16) -> Result<Self, NetError> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Reply),
            _ => Err(NetError::Unsupported),
        }
    }
}

/// A fixed Ethernet/IPv4 ARP packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet {
    operation: Operation,
    sender_hardware: MacAddress,
    sender_protocol: Ipv4Address,
    target_hardware: MacAddress,
    target_protocol: Ipv4Address,
}

impl Packet {
    pub const fn new(
        operation: Operation,
        sender_hardware: MacAddress,
        sender_protocol: Ipv4Address,
        target_hardware: MacAddress,
        target_protocol: Ipv4Address,
    ) -> Self {
        Self { operation, sender_hardware, sender_protocol, target_hardware, target_protocol }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() < LEN {
            return Err(NetError::Malformed);
        }
        if bytes[0..2] != 1u16.to_be_bytes()
            || bytes[2..4] != 0x0800u16.to_be_bytes()
            || bytes[4] != 6
            || bytes[5] != 4
        {
            return Err(NetError::Unsupported);
        }
        Ok(Self::new(
            Operation::parse(u16::from_be_bytes(bytes[6..8].try_into().unwrap()))?,
            MacAddress::new(bytes[8..14].try_into().unwrap()),
            Ipv4Address::from_octets(bytes[14..18].try_into().unwrap()),
            MacAddress::new(bytes[18..24].try_into().unwrap()),
            Ipv4Address::from_octets(bytes[24..28].try_into().unwrap()),
        ))
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, NetError> {
        if bytes.len() < LEN {
            return Err(NetError::Buffer);
        }
        bytes[0..2].copy_from_slice(&1u16.to_be_bytes());
        bytes[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
        bytes[4] = 6;
        bytes[5] = 4;
        bytes[6..8].copy_from_slice(&self.operation.value().to_be_bytes());
        bytes[8..14].copy_from_slice(&self.sender_hardware.octets());
        bytes[14..18].copy_from_slice(&self.sender_protocol.octets());
        bytes[18..24].copy_from_slice(&self.target_hardware.octets());
        bytes[24..28].copy_from_slice(&self.target_protocol.octets());
        Ok(LEN)
    }

    pub const fn operation(self) -> Operation {
        self.operation
    }

    pub const fn sender_hardware(self) -> MacAddress {
        self.sender_hardware
    }

    pub const fn sender_protocol(self) -> Ipv4Address {
        self.sender_protocol
    }

    pub const fn target_hardware(self) -> MacAddress {
        self.target_hardware
    }

    pub const fn target_protocol(self) -> Ipv4Address {
        self.target_protocol
    }
}
