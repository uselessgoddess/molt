use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::ring::{IoRing, RequestId, Submission};
use molt_net::arp::{Operation, Packet as Arp};
use molt_net::eth::{EtherType, Frame};
use molt_net::icmpv6::{self, Message};
use molt_net::ipv4::Packet as Ipv4;
use molt_net::ipv6::Packet as Ipv6;
use molt_net::{
    Config, Ip, IpAddr, IpDone, IpError, IpOp, Ipv4Addr, Ipv6Addr, Link, LinkError, MacAddr,
    NetError, addr,
};

const OWNER: CellId = CellId::new(7);
const LOCAL_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const LOCAL_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
const PEER_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2);
const LOCAL_MAC: MacAddr = MacAddr::new([0x02, 0, 0, 0, 0, 1]);
const PEER_MAC: MacAddr = MacAddr::new([0x52, 0x55, 0x0a, 0, 2, 2]);

#[derive(Default)]
struct Capture {
    frames: Vec<Vec<u8>>,
}

impl Link for Capture {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.frames.push(frame.to_vec());
        Ok(())
    }

    fn receive(&mut self, _frame: &mut [u8]) -> Result<Option<usize>, LinkError> {
        Ok(None)
    }
}

fn config_v4() -> Config {
    Config::new(LOCAL_MAC, IpAddr::V4(LOCAL_V4), 24, IpAddr::V4(PEER_V4))
}

fn config_v6() -> Config {
    Config::new(LOCAL_MAC, IpAddr::V6(LOCAL_V6), 64, IpAddr::V6(PEER_V6))
}

/// One ICMPv6 frame from the peer, at the hop limit the caller wants tested.
fn icmp(dst: Ipv6Addr, message: Message<'_>, hops: u8) -> Result<Vec<u8>, NetError> {
    let mut payload = [0u8; 64];
    let len = message.emit(PEER_V6, dst, &mut payload)?;
    let mut packet = [0u8; 128];
    let len =
        Ipv6::new(PEER_V6, dst, icmpv6::PROTOCOL, &payload[..len]).hops(hops).emit(&mut packet)?;
    let to = if dst.is_multicast() { MacAddr::multicast(dst) } else { LOCAL_MAC };
    let mut frame = [0u8; 160];
    let len = Frame::new(to, PEER_MAC, EtherType::Ipv6, &packet[..len]).emit(&mut frame)?;
    Ok(frame[..len].to_vec())
}

/// Takes apart an IPv6 frame the service put on the link.
fn sent(frame: &[u8]) -> Result<(MacAddr, Ipv6Addr, Ipv6Addr, Vec<u8>), NetError> {
    let frame = Frame::parse(frame)?;
    assert_eq!(frame.ether_type(), EtherType::Ipv6);
    let packet = Ipv6::parse(frame.payload())?;
    Ok((frame.dst(), packet.src(), packet.dst(), packet.payload().to_vec()))
}

#[test]
fn protocol_is_exclusive() -> Result<(), IpError> {
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 4>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v4());
    client.try_submit(Submission::new(RequestId::new(1), IpOp::Bind { protocol: 17 })).unwrap();
    client.try_submit(Submission::new(RequestId::new(2), IpOp::Bind { protocol: 17 })).unwrap();

    assert_eq!(ip.serve(OWNER, &mut driver, &mut buffers), 2);
    assert!(matches!(
        client.try_completion().map(|done| done.into_result()),
        Some(Ok(IpDone::Bound(_)))
    ));
    assert_eq!(client.try_completion().map(|done| done.into_result()), Some(Err(IpError::Bound)));
    Ok(())
}

#[test]
fn send_waits_for_arp() -> Result<(), IpError> {
    let mut payload = *b"udp";
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 4>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_read(OWNER, &mut payload).unwrap();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v4());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Send {
        endpoint,
        to: IpAddr::V4(PEER_V4),
        payload: BufferOperation::new(buffer, 0, 3),
    };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();

    assert_eq!(ip.serve(OWNER, &mut driver, &mut buffers), 0);
    assert!(client.try_completion().is_none());
    assert_eq!(ip.link().frames.len(), 1);
    let request = Frame::parse(&ip.link().frames[0])?;
    assert_eq!(request.ether_type(), EtherType::Arp);

    let mut arp = [0u8; 28];
    Arp::new(Operation::Reply, PEER_MAC, PEER_V4, LOCAL_MAC, LOCAL_V4).emit(&mut arp)?;
    let mut frame = [0u8; 64];
    let len = Frame::new(LOCAL_MAC, PEER_MAC, EtherType::Arp, &arp).emit(&mut frame)?;
    ip.ingest(&frame[..len], &mut driver, &mut buffers)?;

    assert_eq!(client.try_completion().map(|done| done.into_result()), Some(Ok(IpDone::Sent(3))));
    let sent = Frame::parse(&ip.link().frames[1])?;
    let packet = Ipv4::parse(sent.payload())?;
    assert_eq!(packet.payload(), b"udp");
    Ok(())
}

#[test]
fn receive_follows_capability() -> Result<(), IpError> {
    let mut target = [0u8; 16];
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 4>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut target).unwrap();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v4());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Recv { endpoint, payload: BufferOperation::new(buffer, 0, 16) };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();
    ip.serve(OWNER, &mut driver, &mut buffers);
    let mut packet = [0u8; 32];
    let packet_len = Ipv4::new(PEER_V4, LOCAL_V4, 17, b"reply").emit(&mut packet)?;
    let mut frame = [0u8; 64];
    let len =
        Frame::new(LOCAL_MAC, PEER_MAC, EtherType::Ipv4, &packet[..packet_len]).emit(&mut frame)?;

    ip.ingest(&frame[..len], &mut driver, &mut buffers)?;

    assert_eq!(
        client.try_completion().map(|done| done.into_result()),
        Some(Ok(IpDone::Received { from: IpAddr::V4(PEER_V4), len: 5 }))
    );
    assert_eq!(&buffers.resolve_write(BufferOperation::new(buffer, 0, 5))?, b"reply");
    Ok(())
}

#[test]
fn send_solicits_neighbor() -> Result<(), IpError> {
    let mut payload = *b"udp";
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 4>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_read(OWNER, &mut payload).unwrap();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v6());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Send {
        endpoint,
        to: IpAddr::V6(PEER_V6),
        payload: BufferOperation::new(buffer, 0, 3),
    };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();

    assert_eq!(ip.serve(OWNER, &mut driver, &mut buffers), 0);
    let (to, src, dst, bytes) = sent(&ip.link().frames[0])?;
    assert_eq!(to, MacAddr::multicast(addr::solicited_node(PEER_V6)));
    assert_eq!(dst, addr::solicited_node(PEER_V6));
    assert_eq!(
        Message::parse(src, dst, &bytes)?,
        Message::Solicitation { target: PEER_V6, source: Some(LOCAL_MAC) }
    );

    let advertisement =
        Message::Advertisement { target: PEER_V6, hardware: Some(PEER_MAC), solicited: true };
    ip.ingest(&icmp(LOCAL_V6, advertisement, icmpv6::HOPS)?, &mut driver, &mut buffers)?;

    assert_eq!(client.try_completion().map(|done| done.into_result()), Some(Ok(IpDone::Sent(3))));
    let (to, _, dst, bytes) = sent(&ip.link().frames[1])?;
    assert_eq!((to, dst, bytes.as_slice()), (PEER_MAC, PEER_V6, b"udp".as_slice()));
    Ok(())
}

#[test]
fn solicitation_gets_advertisement() -> Result<(), IpError> {
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 2>::new();
    let (_, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v6());
    let solicitation = Message::Solicitation { target: LOCAL_V6, source: Some(PEER_MAC) };

    let frame = icmp(addr::solicited_node(LOCAL_V6), solicitation, icmpv6::HOPS)?;
    assert!(ip.ingest(&frame, &mut driver, &mut buffers)?);

    let (to, src, dst, bytes) = sent(&ip.link().frames[0])?;
    assert_eq!((to, dst), (PEER_MAC, PEER_V6));
    assert_eq!(
        Message::parse(src, dst, &bytes)?,
        Message::Advertisement { target: LOCAL_V6, hardware: Some(LOCAL_MAC), solicited: true }
    );
    Ok(())
}

#[test]
fn discovery_needs_full_hops() -> Result<(), IpError> {
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 2>::new();
    let (_, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v6());
    let solicitation = Message::Solicitation { target: LOCAL_V6, source: Some(PEER_MAC) };

    let frame = icmp(addr::solicited_node(LOCAL_V6), solicitation, 64)?;
    assert!(!ip.ingest(&frame, &mut driver, &mut buffers)?);

    assert!(ip.link().frames.is_empty());
    Ok(())
}

#[test]
fn echo_gets_reply() -> Result<(), IpError> {
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 2>::new();
    let (_, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v6());
    let request = Message::EchoRequest { id: 1, seq: 2, data: b"ping" };

    assert!(ip.ingest(&icmp(LOCAL_V6, request, 64)?, &mut driver, &mut buffers)?);

    let (to, src, dst, bytes) = sent(&ip.link().frames[0])?;
    assert_eq!((to, src, dst), (PEER_MAC, LOCAL_V6, PEER_V6));
    assert_eq!(
        Message::parse(src, dst, &bytes)?,
        Message::EchoReply { id: 1, seq: 2, data: b"ping" }
    );
    Ok(())
}

#[test]
fn ipv6_receive_follows_capability() -> Result<(), IpError> {
    let mut target = [0u8; 16];
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 4>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut target).unwrap();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config_v6());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Recv { endpoint, payload: BufferOperation::new(buffer, 0, 16) };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();
    ip.serve(OWNER, &mut driver, &mut buffers);
    let mut packet = [0u8; 64];
    let packet_len = Ipv6::new(PEER_V6, LOCAL_V6, 17, b"reply").emit(&mut packet)?;
    let mut frame = [0u8; 96];
    let len =
        Frame::new(LOCAL_MAC, PEER_MAC, EtherType::Ipv6, &packet[..packet_len]).emit(&mut frame)?;

    ip.ingest(&frame[..len], &mut driver, &mut buffers)?;

    assert_eq!(
        client.try_completion().map(|done| done.into_result()),
        Some(Ok(IpDone::Received { from: IpAddr::V6(PEER_V6), len: 5 }))
    );
    assert_eq!(&buffers.resolve_write(BufferOperation::new(buffer, 0, 5))?, b"reply");
    Ok(())
}

#[test]
fn families_cannot_cross() -> Result<(), IpError> {
    let mut payload = *b"udp";
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 2>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_read(OWNER, &mut payload).unwrap();
    let mut ip = Ip::<_, 1>::new(Capture::default(), config_v4());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Send {
        endpoint,
        to: IpAddr::V6(Ipv6Addr::LOCALHOST),
        payload: BufferOperation::new(buffer, 0, 3),
    };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();

    ip.serve(OWNER, &mut driver, &mut buffers);

    assert_eq!(
        client.try_completion().map(|done| done.into_result()),
        Some(Err(IpError::Unsupported))
    );
    assert!(ip.link().frames.is_empty());
    Ok(())
}
