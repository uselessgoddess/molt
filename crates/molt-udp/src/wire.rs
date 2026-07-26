//! UDP wire format.

use molt_net::address::Ipv4Address;
use molt_net::checksum;

use crate::UdpError;

const HEADER: usize = 8;

/// An IPv4 address and UDP port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Endpoint {
    address: Ipv4Address,
    port: u16,
}

impl Endpoint {
    pub const fn new(address: Ipv4Address, port: u16) -> Self {
        Self { address, port }
    }

    pub const fn address(self) -> Ipv4Address {
        self.address
    }

    pub const fn port(self) -> u16 {
        self.port
    }
}

/// A UDP header and borrowed payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Datagram<'a> {
    source: Endpoint,
    destination: Endpoint,
    payload: &'a [u8],
}

impl<'a> Datagram<'a> {
    pub const fn new(source: Endpoint, destination: Endpoint, payload: &'a [u8]) -> Self {
        Self { source, destination, payload }
    }

    pub fn parse(
        source: Ipv4Address,
        destination: Ipv4Address,
        bytes: &'a [u8],
    ) -> Result<Self, UdpError> {
        if bytes.len() < HEADER {
            return Err(UdpError::Malformed);
        }
        let len = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
        if len < HEADER || bytes.len() < len {
            return Err(UdpError::Malformed);
        }
        let transmitted = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
        if transmitted != 0 && checksum(source, destination, &bytes[..len]) != 0 {
            return Err(UdpError::Checksum);
        }
        Ok(Self::new(
            Endpoint::new(source, u16::from_be_bytes(bytes[0..2].try_into().unwrap())),
            Endpoint::new(destination, u16::from_be_bytes(bytes[2..4].try_into().unwrap())),
            &bytes[HEADER..len],
        ))
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, UdpError> {
        let len = HEADER.checked_add(self.payload.len()).ok_or(UdpError::Buffer)?;
        let len = u16::try_from(len).map_err(|_| UdpError::Buffer)?;
        if bytes.len() < len as usize {
            return Err(UdpError::Buffer);
        }
        bytes[0..2].copy_from_slice(&self.source.port.to_be_bytes());
        bytes[2..4].copy_from_slice(&self.destination.port.to_be_bytes());
        bytes[4..6].copy_from_slice(&len.to_be_bytes());
        bytes[6..8].fill(0);
        bytes[HEADER..len as usize].copy_from_slice(self.payload);
        let sum = checksum(self.source.address, self.destination.address, &bytes[..len as usize]);
        bytes[6..8].copy_from_slice(&if sum == 0 { u16::MAX } else { sum }.to_be_bytes());
        Ok(len as usize)
    }

    pub const fn source(&self) -> Endpoint {
        self.source
    }

    pub const fn destination(&self) -> Endpoint {
        self.destination
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

fn checksum(source: Ipv4Address, destination: Ipv4Address, bytes: &[u8]) -> u16 {
    let len = (bytes.len() as u16).to_be_bytes();
    let pseudo = [
        source.octets()[0],
        source.octets()[1],
        source.octets()[2],
        source.octets()[3],
        destination.octets()[0],
        destination.octets()[1],
        destination.octets()[2],
        destination.octets()[3],
        0,
        17,
        len[0],
        len[1],
    ];
    checksum::compute_parts(&[&pseudo, bytes])
}
