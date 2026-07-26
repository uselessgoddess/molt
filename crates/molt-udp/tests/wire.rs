use molt_net::address::Ipv4Address;
use molt_udp::{Datagram, Endpoint, UdpError};

const LOCAL: Endpoint = Endpoint::new(Ipv4Address::new(10, 0, 2, 15), 49152);
const PEER: Endpoint = Endpoint::new(Ipv4Address::new(10, 0, 2, 2), 7);

#[test]
fn datagram_roundtrips() -> Result<(), UdpError> {
    let mut bytes = [0u8; 64];
    let datagram = Datagram::new(LOCAL, PEER, b"molt");

    let len = datagram.emit(&mut bytes)?;
    let parsed = Datagram::parse(LOCAL.address(), PEER.address(), &bytes[..len])?;

    assert_eq!(parsed, datagram);
    Ok(())
}

#[test]
fn checksum_rejects_damage() -> Result<(), UdpError> {
    let mut bytes = [0u8; 64];
    let len = Datagram::new(LOCAL, PEER, b"molt").emit(&mut bytes)?;
    bytes[8] ^= 1;

    assert_eq!(
        Datagram::parse(LOCAL.address(), PEER.address(), &bytes[..len]),
        Err(UdpError::Checksum)
    );
    Ok(())
}
