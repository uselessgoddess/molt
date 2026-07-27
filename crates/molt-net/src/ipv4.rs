//! IPv4 packets without options or fragmentation.

use crate::address::Ipv4Addr;
use crate::{NetError, checksum};

const HEADER: usize = 20;

/// An IPv4 header and borrowed protocol payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet<'a> {
    source: Ipv4Addr,
    destination: Ipv4Addr,
    protocol: u8,
    payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub const fn new(
        source: Ipv4Addr,
        destination: Ipv4Addr,
        protocol: u8,
        payload: &'a [u8],
    ) -> Self {
        Self { source, destination, protocol, payload }
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self, NetError> {
        if bytes.len() < HEADER || bytes[0] >> 4 != 4 {
            return Err(NetError::Malformed);
        }
        let header = ((bytes[0] & 0x0f) as usize) * 4;
        if header < HEADER || bytes.len() < header {
            return Err(NetError::Malformed);
        }
        let total = u16::from_be_bytes(bytes[2..4].try_into().unwrap()) as usize;
        if total < header || bytes.len() < total {
            return Err(NetError::Malformed);
        }
        if !checksum::valid(&bytes[..header]) {
            return Err(NetError::Checksum);
        }
        let fragment = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
        if fragment & 0x3fff != 0 {
            return Err(NetError::Fragmented);
        }
        Ok(Self::new(
            Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]),
            Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]),
            bytes[9],
            &bytes[header..total],
        ))
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, NetError> {
        let total = HEADER.checked_add(self.payload.len()).ok_or(NetError::Buffer)?;
        let total = u16::try_from(total).map_err(|_| NetError::Buffer)?;
        if bytes.len() < total as usize {
            return Err(NetError::Buffer);
        }
        let header = &mut bytes[..HEADER];
        header.fill(0);
        header[0] = 0x45;
        header[2..4].copy_from_slice(&total.to_be_bytes());
        header[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        header[8] = 64;
        header[9] = self.protocol;
        header[12..16].copy_from_slice(&self.source.octets());
        header[16..20].copy_from_slice(&self.destination.octets());
        checksum::set(header, 10);
        bytes[HEADER..total as usize].copy_from_slice(self.payload);
        Ok(total as usize)
    }

    pub const fn source(&self) -> Ipv4Addr {
        self.source
    }

    pub const fn destination(&self) -> Ipv4Addr {
        self.destination
    }

    pub const fn protocol(&self) -> u8 {
        self.protocol
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}
