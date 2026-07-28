use molt_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use molt_udp::{Datagram, Endpoint, UdpError};

const LOCAL: Endpoint = Endpoint::new(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)), 49152);
const PEER: Endpoint = Endpoint::new(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)), 7);

#[test]
fn datagram_roundtrips() -> Result<(), UdpError> {
    let mut bytes = [0u8; 64];
    let datagram = Datagram::new(LOCAL, PEER, b"molt");

    let len = datagram.emit(&mut bytes)?;
    let parsed = Datagram::parse(LOCAL.addr(), PEER.addr(), &bytes[..len])?;

    assert_eq!(parsed, datagram);
    Ok(())
}

#[test]
fn checksum_rejects_damage() -> Result<(), UdpError> {
    let mut bytes = [0u8; 64];
    let len = Datagram::new(LOCAL, PEER, b"molt").emit(&mut bytes)?;
    bytes[8] ^= 1;

    assert_eq!(Datagram::parse(LOCAL.addr(), PEER.addr(), &bytes[..len]), Err(UdpError::Checksum));
    Ok(())
}

#[test]
fn ipv6_datagram_roundtrips() -> Result<(), UdpError> {
    let local = Endpoint::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 49152);
    let peer = Endpoint::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 7);
    let mut bytes = [0u8; 64];

    let len = Datagram::new(local, peer, b"molt").emit(&mut bytes)?;
    let parsed = Datagram::parse(local.addr(), peer.addr(), &bytes[..len])?;

    assert_eq!(parsed, Datagram::new(local, peer, b"molt"));
    Ok(())
}

#[test]
fn ipv6_requires_checksum() {
    let src = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let dst = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    let bytes = [0, 7, 0, 8, 0, 8, 0, 0];

    assert_eq!(Datagram::parse(src, dst, &bytes), Err(UdpError::Checksum));
}
