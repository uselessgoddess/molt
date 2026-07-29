//! IPv6 packets without extension headers.

use crate::NetError;
use crate::addr::Ipv6Addr;

/// The next-header value that ends the chain with no payload protocol.
pub const NO_NEXT_HEADER: u8 = 59;

/// An IPv6 header and borrowed protocol payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet<'a> {
    src: Ipv6Addr,
    dst: Ipv6Addr,
    next_header: u8,
    hop_limit: u8,
    payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub const HEADER: usize = 40;

    /// A packet with the default hop limit.
    pub const fn new(src: Ipv6Addr, dst: Ipv6Addr, next_header: u8, payload: &'a [u8]) -> Self {
        Self { src, dst, next_header, hop_limit: 64, payload }
    }

    /// Sets the hop limit, which discovery pins to 255 so a router cannot
    /// forward a neighbour message onto a link it did not come from.
    pub const fn hops(self, hop_limit: u8) -> Self {
        Self { hop_limit, ..self }
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self, NetError> {
        if bytes.len() < Self::HEADER || bytes[0] >> 4 != 6 {
            return Err(NetError::Malformed);
        }
        let length = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
        let total = Self::HEADER.checked_add(length).ok_or(NetError::Malformed)?;
        if bytes.len() < total {
            return Err(NetError::Malformed);
        }
        // Every extension header is refused rather than skipped: a header the
        // parser walks past is a header nothing above it can police.
        match bytes[6] {
            44 => return Err(NetError::Fragmented),
            0 | 43 | 51 | 60 => return Err(NetError::Unsupported),
            _ => {}
        }
        Ok(Self {
            src: address(&bytes[8..24]),
            dst: address(&bytes[24..40]),
            next_header: bytes[6],
            hop_limit: bytes[7],
            payload: &bytes[Self::HEADER..total],
        })
    }

    pub fn emit(&self, bytes: &mut [u8]) -> Result<usize, NetError> {
        let length = u16::try_from(self.payload.len()).map_err(|_| NetError::Buffer)?;
        let total = Self::HEADER + self.payload.len();
        if bytes.len() < total {
            return Err(NetError::Buffer);
        }
        let header = &mut bytes[..Self::HEADER];
        header.fill(0);
        header[0] = 0x60;
        header[4..6].copy_from_slice(&length.to_be_bytes());
        header[6] = self.next_header;
        header[7] = self.hop_limit;
        header[8..24].copy_from_slice(&self.src.octets());
        header[24..40].copy_from_slice(&self.dst.octets());
        bytes[Self::HEADER..total].copy_from_slice(self.payload);
        Ok(total)
    }

    pub const fn src(&self) -> Ipv6Addr {
        self.src
    }

    pub const fn dst(&self) -> Ipv6Addr {
        self.dst
    }

    pub const fn next_header(&self) -> u8 {
        self.next_header
    }

    pub const fn hop_limit(&self) -> u8 {
        self.hop_limit
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

fn address(bytes: &[u8]) -> Ipv6Addr {
    Ipv6Addr::from(<[u8; 16]>::try_from(bytes).unwrap())
}
