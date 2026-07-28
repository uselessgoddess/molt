use molt_net::NetError;
use molt_net::addr::{Ipv4Addr, MacAddr};
use molt_net::arp::{Operation, Packet as Arp};
use molt_net::ethernet::{EtherType, Frame};
use molt_net::ipv4::Packet as Ipv4;

const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const LOCAL_MAC: MacAddr = MacAddr::new([0x02, 0, 0, 0, 0, 1]);
const PEER_MAC: MacAddr = MacAddr::new([0x52, 0x55, 0x0a, 0, 2, 2]);

#[test]
fn ethernet_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 64];
    let frame = Frame::new(PEER_MAC, LOCAL_MAC, EtherType::Ipv4, b"molt");

    let len = frame.emit(&mut bytes)?;
    let parsed = Frame::parse(&bytes[..len])?;

    assert_eq!(parsed, frame);
    Ok(())
}

#[test]
fn ethernet_recognizes_ipv6() -> Result<(), NetError> {
    let mut bytes = [0u8; 64];
    let frame = Frame::new(PEER_MAC, LOCAL_MAC, EtherType::Ipv6, b"molt");

    let len = frame.emit(&mut bytes)?;
    let parsed = Frame::parse(&bytes[..len])?;

    assert_eq!(parsed, frame);
    Ok(())
}

#[test]
fn arp_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 28];
    let packet = Arp::new(Operation::Reply, LOCAL_MAC, LOCAL_IP, PEER_MAC, PEER_IP);

    packet.emit(&mut bytes)?;
    let parsed = Arp::parse(&bytes)?;

    assert_eq!(parsed, packet);
    Ok(())
}

#[test]
fn ipv4_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 64];
    let packet = Ipv4::new(LOCAL_IP, PEER_IP, 17, b"datagram");

    let len = packet.emit(&mut bytes)?;
    let parsed = Ipv4::parse(&bytes[..len])?;

    assert_eq!(parsed, packet);
    Ok(())
}

#[test]
fn fragments_fail_closed() {
    let mut bytes = [0x45, 0, 0, 20, 0, 1, 0x20, 0, 64, 17, 0, 0, 10, 0, 2, 2, 10, 0, 2, 15];
    molt_net::checksum::set(&mut bytes, 10);

    assert_eq!(Ipv4::parse(&bytes), Err(NetError::Fragmented));
}
