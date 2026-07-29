//! UDP wire format.

use molt_net::{Endpoint, IpAddr, checksum};

use crate::UdpError;

/// The IP protocol number carrying datagrams.
pub const PROTOCOL: u8 = 17;

/// A UDP header and borrowed payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Datagram<'a> {
    src: Endpoint,
    dst: Endpoint,
    payload: &'a [u8],
}

impl<'a> Datagram<'a> {
    pub const HEADER: usize = 8;

    pub const fn new(src: Endpoint, dst: Endpoint, payload: &'a [u8]) -> Self {
        Self { src, dst, payload }
    }

    pub fn parse(src: IpAddr, dst: IpAddr, bytes: &'a [u8]) -> Result<Self, UdpError> {
        if bytes.len() < Self::HEADER {
            return Err(UdpError::Malformed);
        }
        let len = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
        if len < Self::HEADER || bytes.len() < len {
            return Err(UdpError::Malformed);
        }
        let transmitted = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
        if transmitted == 0 && matches!(src, IpAddr::V6(_)) {
            return Err(UdpError::Checksum);
        }
        if transmitted != 0 && checksum(src, dst, &bytes[..len])? != 0 {
            return Err(UdpError::Checksum);
        }
        Ok(Self::new(
            Endpoint::new(src, u16::from_be_bytes(bytes[0..2].try_into().unwrap())),
            Endpoint::new(dst, u16::from_be_bytes(bytes[2..4].try_into().unwrap())),
            &bytes[Self::HEADER..len],
        ))
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, UdpError> {
        let len = Self::HEADER.checked_add(self.payload.len()).ok_or(UdpError::Buffer)?;
        let len = u16::try_from(len).map_err(|_| UdpError::Buffer)?;
        if bytes.len() < len as usize {
            return Err(UdpError::Buffer);
        }
        bytes[0..2].copy_from_slice(&self.src.port().to_be_bytes());
        bytes[2..4].copy_from_slice(&self.dst.port().to_be_bytes());
        bytes[4..6].copy_from_slice(&len.to_be_bytes());
        bytes[6..8].fill(0);
        bytes[Self::HEADER..len as usize].copy_from_slice(self.payload);
        let sum = checksum(self.src.addr(), self.dst.addr(), &bytes[..len as usize])?;
        bytes[6..8].copy_from_slice(&if sum == 0 { u16::MAX } else { sum }.to_be_bytes());
        Ok(len as usize)
    }

    pub const fn src(&self) -> Endpoint {
        self.src
    }

    pub const fn dst(&self) -> Endpoint {
        self.dst
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

fn checksum(src: IpAddr, dst: IpAddr, bytes: &[u8]) -> Result<u16, UdpError> {
    match (src, dst) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => Ok(checksum::over_ipv4(src, dst, PROTOCOL, bytes)),
        (IpAddr::V6(src), IpAddr::V6(dst)) => Ok(checksum::over_ipv6(src, dst, PROTOCOL, bytes)),
        _ => Err(UdpError::Malformed),
    }
}
