use molt_net::arp::{Operation, Packet as Arp};
use molt_net::eth::{EtherType, Frame};
use molt_net::icmpv6::{self, Message};
use molt_net::ipv4::Packet as Ipv4;
use molt_net::ipv6::Packet as Ipv6;
use molt_net::{Ipv4Addr, Ipv6Addr, MacAddr, NetError, addr};

const LOCAL_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const LOCAL_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
const PEER_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2);
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
    let packet = Arp::new(Operation::Reply, LOCAL_MAC, LOCAL_V4, PEER_MAC, PEER_V4);

    packet.emit(&mut bytes)?;
    let parsed = Arp::parse(&bytes)?;

    assert_eq!(parsed, packet);
    Ok(())
}

#[test]
fn ipv4_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 64];
    let packet = Ipv4::new(LOCAL_V4, PEER_V4, 17, b"datagram");

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

#[test]
fn ipv6_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 64];
    let packet = Ipv6::new(LOCAL_V6, PEER_V6, 17, b"datagram").hops(icmpv6::HOPS);

    let len = packet.emit(&mut bytes)?;
    let parsed = Ipv6::parse(&bytes[..len])?;

    assert_eq!(parsed, packet);
    Ok(())
}

#[test]
fn extensions_fail_closed() -> Result<(), NetError> {
    let mut bytes = [0u8; 48];
    Ipv6::new(LOCAL_V6, PEER_V6, 17, b"x").emit(&mut bytes)?;
    bytes[6] = 44;

    assert_eq!(Ipv6::parse(&bytes[..41]), Err(NetError::Fragmented));
    Ok(())
}

#[test]
fn echo_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 32];
    let message = Message::EchoRequest { id: 7, seq: 3, data: b"ping" };

    let len = message.emit(LOCAL_V6, PEER_V6, &mut bytes)?;
    let parsed = Message::parse(LOCAL_V6, PEER_V6, &bytes[..len])?;

    assert_eq!(parsed, message);
    Ok(())
}

#[test]
fn discovery_roundtrips() -> Result<(), NetError> {
    let mut bytes = [0u8; 32];
    let message =
        Message::Advertisement { target: LOCAL_V6, hardware: Some(LOCAL_MAC), solicited: true };

    let len = message.emit(LOCAL_V6, PEER_V6, &mut bytes)?;
    let parsed = Message::parse(LOCAL_V6, PEER_V6, &bytes[..len])?;

    assert_eq!(parsed, message);
    Ok(())
}

#[test]
fn checksum_binds_addresses() -> Result<(), NetError> {
    let mut bytes = [0u8; 32];
    let message = Message::Solicitation { target: PEER_V6, source: Some(LOCAL_MAC) };

    let len = message.emit(LOCAL_V6, PEER_V6, &mut bytes)?;

    let elsewhere = addr::solicited_node(PEER_V6);
    assert_eq!(Message::parse(LOCAL_V6, elsewhere, &bytes[..len]), Err(NetError::Checksum));
    Ok(())
}

#[test]
fn groups_map_onto_link() {
    assert_eq!(
        addr::link_local(PEER_MAC),
        Ipv6Addr::new(0xfe80, 0, 0, 0, 0x5055, 0x0aff, 0xfe00, 0x0202)
    );
    assert_eq!(addr::solicited_node(PEER_V6), Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 2));
    assert_eq!(
        MacAddr::multicast(addr::solicited_node(PEER_V6)),
        MacAddr::new([0x33, 0x33, 0xff, 0, 0, 2])
    );
}
