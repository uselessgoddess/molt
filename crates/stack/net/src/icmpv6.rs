//! ICMPv6 echo and the two neighbour discovery messages a host needs.

use crate::{Ipv6Addr, MacAddr, NetError, checksum};

/// The next-header value carrying these messages.
pub const PROTOCOL: u8 = 58;

/// The hop limit discovery is sent with and accepted at (RFC 4861 §3.1).
pub const HOPS: u8 = 255;

const ECHO_REQUEST: u8 = 128;
const ECHO_REPLY: u8 = 129;
const SOLICITATION: u8 = 135;
const ADVERTISEMENT: u8 = 136;

const SOURCE_LINK: u8 = 1;
const TARGET_LINK: u8 = 2;

const SOLICITED: u8 = 0x40;
const OVERRIDE: u8 = 0x20;

/// One ICMPv6 message this stack speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Message<'a> {
    EchoRequest {
        id: u16,
        seq: u16,
        data: &'a [u8],
    },
    EchoReply {
        id: u16,
        seq: u16,
        data: &'a [u8],
    },
    /// Asks which link address answers for `target`.
    Solicitation {
        target: Ipv6Addr,
        source: Option<MacAddr>,
    },
    /// Answers that `hardware` does, either unprompted or because it was asked.
    Advertisement {
        target: Ipv6Addr,
        hardware: Option<MacAddr>,
        solicited: bool,
    },
}

impl<'a> Message<'a> {
    /// Reads a message whose checksum covers the addresses it arrived between.
    pub fn parse(src: Ipv6Addr, dst: Ipv6Addr, bytes: &'a [u8]) -> Result<Self, NetError> {
        if bytes.len() < 8 {
            return Err(NetError::Malformed);
        }
        if checksum::over_ipv6(src, dst, PROTOCOL, bytes) != 0 {
            return Err(NetError::Checksum);
        }
        if bytes[1] != 0 {
            return Err(NetError::Unsupported);
        }
        match bytes[0] {
            kind @ (ECHO_REQUEST | ECHO_REPLY) => {
                let id = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
                let seq = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
                let data = &bytes[8..];
                Ok(match kind {
                    ECHO_REQUEST => Self::EchoRequest { id, seq, data },
                    _ => Self::EchoReply { id, seq, data },
                })
            }
            kind @ (SOLICITATION | ADVERTISEMENT) => {
                if bytes.len() < 24 {
                    return Err(NetError::Malformed);
                }
                let target = Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).unwrap());
                match kind {
                    SOLICITATION => Ok(Self::Solicitation {
                        target,
                        source: option(&bytes[24..], SOURCE_LINK)?,
                    }),
                    _ => Ok(Self::Advertisement {
                        target,
                        hardware: option(&bytes[24..], TARGET_LINK)?,
                        solicited: bytes[4] & SOLICITED != 0,
                    }),
                }
            }
            _ => Err(NetError::Unsupported),
        }
    }

    /// Writes the message and the checksum binding it to `src` and `dst`.
    pub fn emit(&self, src: Ipv6Addr, dst: Ipv6Addr, bytes: &mut [u8]) -> Result<usize, NetError> {
        let len = self.bytes();
        if bytes.len() < len {
            return Err(NetError::Buffer);
        }
        let bytes = &mut bytes[..len];
        bytes.fill(0);
        match *self {
            Self::EchoRequest { id, seq, data } | Self::EchoReply { id, seq, data } => {
                bytes[0] = if matches!(self, Self::EchoRequest { .. }) {
                    ECHO_REQUEST
                } else {
                    ECHO_REPLY
                };
                bytes[4..6].copy_from_slice(&id.to_be_bytes());
                bytes[6..8].copy_from_slice(&seq.to_be_bytes());
                bytes[8..].copy_from_slice(data);
            }
            Self::Solicitation { target, source } => {
                bytes[0] = SOLICITATION;
                bytes[8..24].copy_from_slice(&target.octets());
                emit_option(&mut bytes[24..], SOURCE_LINK, source);
            }
            Self::Advertisement { target, hardware, solicited } => {
                bytes[0] = ADVERTISEMENT;
                // Override is always set: this host is the address's owner, so
                // a cached entry that disagrees is stale by construction.
                bytes[4] = OVERRIDE | if solicited { SOLICITED } else { 0 };
                bytes[8..24].copy_from_slice(&target.octets());
                emit_option(&mut bytes[24..], TARGET_LINK, hardware);
            }
        }
        let sum = checksum::over_ipv6(src, dst, PROTOCOL, bytes);
        bytes[2..4].copy_from_slice(&sum.to_be_bytes());
        Ok(len)
    }

    /// How much room [`emit`](Self::emit) needs.
    pub const fn bytes(&self) -> usize {
        match *self {
            Self::EchoRequest { data, .. } | Self::EchoReply { data, .. } => 8 + data.len(),
            Self::Solicitation { source: link, .. }
            | Self::Advertisement { hardware: link, .. } => 24 + if link.is_some() { 8 } else { 0 },
        }
    }
}

/// Finds one link-layer address option, refusing a truncated option chain.
fn option(mut bytes: &[u8], want: u8) -> Result<Option<MacAddr>, NetError> {
    let mut found = None;
    while !bytes.is_empty() {
        if bytes.len() < 2 || bytes[1] == 0 {
            return Err(NetError::Malformed);
        }
        let len = bytes[1] as usize * 8;
        if bytes.len() < len {
            return Err(NetError::Malformed);
        }
        if bytes[0] == want && len == 8 && found.is_none() {
            found = Some(MacAddr::new(bytes[2..8].try_into().unwrap()));
        }
        bytes = &bytes[len..];
    }
    Ok(found)
}

fn emit_option(bytes: &mut [u8], kind: u8, link: Option<MacAddr>) {
    let Some(link) = link else { return };
    bytes[0] = kind;
    bytes[1] = 1;
    bytes[2..8].copy_from_slice(&link.octets());
}
