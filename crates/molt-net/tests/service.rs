use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::ring::{IoRing, RequestId, Submission};
use molt_net::address::{Ipv4Address, MacAddress};
use molt_net::arp::{Operation, Packet as Arp};
use molt_net::ethernet::{EtherType, Frame};
use molt_net::ipv4::Packet as Ipv4;
use molt_net::{Config, Ip, IpDone, IpError, IpOp, Link, LinkError};

const OWNER: CellId = CellId::new(7);
const LOCAL_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const PEER_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const LOCAL_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 1]);
const PEER_MAC: MacAddress = MacAddress::new([0x52, 0x55, 0x0a, 0, 2, 2]);

#[derive(Default)]
struct Capture {
    frames: Vec<Vec<u8>>,
}

impl Link for Capture {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.frames.push(frame.to_vec());
        Ok(())
    }
}

fn config() -> Config {
    Config::new(LOCAL_MAC, LOCAL_IP, 24, PEER_IP)
}

#[test]
fn protocol_is_exclusive() -> Result<(), IpError> {
    let mut ring = IoRing::<IpOp, Result<IpDone, IpError>, 4>::new();
    let (mut client, mut driver) = ring.split();
    let mut buffers = BufferRegistry::<1>::new();
    let mut ip = Ip::<_, 2>::new(Capture::default(), config());
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
    let mut ip = Ip::<_, 2>::new(Capture::default(), config());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Send { endpoint, to: PEER_IP, payload: BufferOperation::new(buffer, 0, 3) };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();

    assert_eq!(ip.serve(OWNER, &mut driver, &mut buffers), 0);
    assert!(client.try_completion().is_none());
    assert_eq!(ip.link().frames.len(), 1);
    let request = Frame::parse(&ip.link().frames[0])?;
    assert_eq!(request.ether_type(), EtherType::Arp);

    let mut arp = [0u8; 28];
    Arp::new(Operation::Reply, PEER_MAC, PEER_IP, LOCAL_MAC, LOCAL_IP).emit(&mut arp)?;
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
    let mut ip = Ip::<_, 2>::new(Capture::default(), config());
    let endpoint = ip.bind(OWNER, 17)?;
    let op = IpOp::Recv { endpoint, payload: BufferOperation::new(buffer, 0, 16) };
    client.try_submit(Submission::new(RequestId::new(1), op)).unwrap();
    ip.serve(OWNER, &mut driver, &mut buffers);
    let mut packet = [0u8; 32];
    let packet_len = Ipv4::new(PEER_IP, LOCAL_IP, 17, b"reply").emit(&mut packet)?;
    let mut frame = [0u8; 64];
    let len =
        Frame::new(LOCAL_MAC, PEER_MAC, EtherType::Ipv4, &packet[..packet_len]).emit(&mut frame)?;

    ip.ingest(&frame[..len], &mut driver, &mut buffers)?;

    assert_eq!(
        client.try_completion().map(|done| done.into_result()),
        Some(Ok(IpDone::Received { from: PEER_IP, len: 5 }))
    );
    assert_eq!(&buffers.resolve_write(BufferOperation::new(buffer, 0, 5))?, b"reply");
    Ok(())
}
