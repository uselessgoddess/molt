//! Ethernet/IPv4 address resolution packets.

use crate::{Ipv4Addr, MacAddr, NetError};

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
    tx_hardware: MacAddr,
    tx_protocol: Ipv4Addr,
    rx_hardware: MacAddr,
    rx_protocol: Ipv4Addr,
}

impl Packet {
    pub const LEN: usize = 28;

    pub const fn new(
        operation: Operation,
        tx_hardware: MacAddr,
        tx_protocol: Ipv4Addr,
        rx_hardware: MacAddr,
        rx_protocol: Ipv4Addr,
    ) -> Self {
        Self { operation, tx_hardware, tx_protocol, rx_hardware, rx_protocol }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, NetError> {
        if bytes.len() < Self::LEN {
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
            MacAddr::new(bytes[8..14].try_into().unwrap()),
            Ipv4Addr::new(bytes[14], bytes[15], bytes[16], bytes[17]),
            MacAddr::new(bytes[18..24].try_into().unwrap()),
            Ipv4Addr::new(bytes[24], bytes[25], bytes[26], bytes[27]),
        ))
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, NetError> {
        if bytes.len() < Self::LEN {
            return Err(NetError::Buffer);
        }
        bytes[0..2].copy_from_slice(&1u16.to_be_bytes());
        bytes[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
        bytes[4] = 6;
        bytes[5] = 4;
        bytes[6..8].copy_from_slice(&self.operation.value().to_be_bytes());
        bytes[8..14].copy_from_slice(&self.tx_hardware.octets());
        bytes[14..18].copy_from_slice(&self.tx_protocol.octets());
        bytes[18..24].copy_from_slice(&self.rx_hardware.octets());
        bytes[24..28].copy_from_slice(&self.rx_protocol.octets());
        Ok(Self::LEN)
    }

    pub const fn operation(self) -> Operation {
        self.operation
    }

    pub const fn tx_hardware(self) -> MacAddr {
        self.tx_hardware
    }

    pub const fn tx_protocol(self) -> Ipv4Addr {
        self.tx_protocol
    }

    pub const fn rx_hardware(self) -> MacAddr {
        self.rx_hardware
    }

    pub const fn rx_protocol(self) -> Ipv4Addr {
        self.rx_protocol
    }
}
