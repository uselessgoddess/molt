use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::cell::Cell;
use molt_core::ring::{IoRing, RequestId, Submission};
use molt_net::addr::{IpAddr, Ipv4Addr, MacAddr};
use molt_net::arp::{Operation, Packet as Arp};
use molt_net::ethernet::{EtherType, Frame};
use molt_net::ipv4::Packet as Ipv4;
use molt_net::{Config, Ip, IpDone, IpError, IpOp, Link, LinkError};
use molt_udp::{Datagram, Endpoint, Scratch, Udp, UdpCell, UdpDone, UdpError, UdpOp, UdpState};

const OWNER: CellId = CellId::new(7);
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
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
}

#[test]
fn datagram_crosses_rings() -> Result<(), UdpError> {
    let mut source = *b"ping";
    let mut target = [0u8; 16];
    let mut tx = [0u8; 1480];
    let mut rx = [0u8; 1480];
    let mut buffers = BufferRegistry::<4>::new();
    let source = buffers.register_read(OWNER, &mut source).unwrap();
    let target = buffers.register_write(OWNER, &mut target).unwrap();
    let tx = buffers.register_read_write(OWNER, &mut tx).unwrap();
    let rx = buffers.register_read_write(OWNER, &mut rx).unwrap();
    let tx = Scratch::new(
        BufferOperation::new(buffers.read_capability(tx)?, 0, 1480),
        BufferOperation::new(buffers.write_capability(tx)?, 0, 1480),
    );
    let rx = Scratch::new(
        BufferOperation::new(buffers.read_capability(rx)?, 0, 1480),
        BufferOperation::new(buffers.write_capability(rx)?, 0, 1480),
    );
    let mut ip_ring = IoRing::<IpOp, Result<IpDone, IpError>, 8>::new();
    let (mut ip_client, mut ip_driver) = ip_ring.split();
    let mut udp_ring = IoRing::<UdpOp, Result<UdpDone, UdpError>, 8>::new();
    let (mut client, mut driver) = udp_ring.split();
    let config = Config::new(LOCAL_MAC, IpAddr::V4(LOCAL_IP), 24, IpAddr::V4(PEER_IP));
    let mut ip = Ip::<_, 2>::new(Capture::default(), config);
    let mut udp = Udp::<2, 2>::new(IpAddr::V4(LOCAL_IP), tx, rx);

    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    client.try_submit(Submission::new(RequestId::new(1), UdpOp::Bind { port: 49152 })).unwrap();
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    let Some(Ok(UdpDone::Bound(socket))) = client.try_completion().map(|done| done.into_result())
    else {
        panic!("UDP bind did not return a socket");
    };

    let send = UdpOp::Send {
        socket,
        to: Endpoint::new(IpAddr::V4(PEER_IP), 7),
        payload: BufferOperation::new(source, 0, 4),
    };
    client.try_submit(Submission::new(RequestId::new(2), send)).unwrap();
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);
    let mut arp = [0u8; 28];
    Arp::new(Operation::Reply, PEER_MAC, PEER_IP, LOCAL_MAC, LOCAL_IP).emit(&mut arp)?;
    let mut frame = [0u8; 64];
    let len = Frame::new(LOCAL_MAC, PEER_MAC, EtherType::Arp, &arp).emit(&mut frame)?;
    ip.ingest(&frame[..len], &mut ip_driver, &mut buffers)?;
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);

    assert_eq!(client.try_completion().map(|done| done.into_result()), Some(Ok(UdpDone::Sent(4))));
    let sent = Frame::parse(&ip.link().frames[1])?;
    let sent = Ipv4::parse(sent.payload())?;
    let sent = Datagram::parse(IpAddr::V4(sent.src()), IpAddr::V4(sent.dst()), sent.payload())?;
    assert_eq!(sent.payload(), b"ping");

    let recv = UdpOp::Recv { socket, payload: BufferOperation::new(target, 0, 16) };
    client.try_submit(Submission::new(RequestId::new(3), recv)).unwrap();
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    let mut datagram = [0u8; 32];
    let datagram_len = Datagram::new(
        Endpoint::new(IpAddr::V4(PEER_IP), 7),
        Endpoint::new(IpAddr::V4(LOCAL_IP), 49152),
        b"pong",
    )
    .emit(&mut datagram)?;
    let mut packet = [0u8; 64];
    let packet_len =
        Ipv4::new(PEER_IP, LOCAL_IP, 17, &datagram[..datagram_len]).emit(&mut packet)?;
    let mut frame = [0u8; 96];
    let len =
        Frame::new(LOCAL_MAC, PEER_MAC, EtherType::Ipv4, &packet[..packet_len]).emit(&mut frame)?;
    ip.ingest(&frame[..len], &mut ip_driver, &mut buffers)?;
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);

    assert_eq!(
        client.try_completion().map(|done| done.into_result()),
        Some(Ok(UdpDone::Received { from: Endpoint::new(IpAddr::V4(PEER_IP), 7), len: 4 }))
    );
    assert_eq!(&buffers.resolve_write(BufferOperation::new(target, 0, 4))?, b"pong");
    Ok(())
}

#[test]
fn restart_stales_reused_socket() -> Result<(), UdpError> {
    let mut tx = [0u8; 32];
    let mut rx = [0u8; 32];
    let mut buffers = BufferRegistry::<2>::new();
    let tx = buffers.register_read_write(OWNER, &mut tx).unwrap();
    let rx = buffers.register_read_write(OWNER, &mut rx).unwrap();
    let state = UdpState::new(
        IpAddr::V4(LOCAL_IP),
        Scratch::from_registered(tx, 32, &buffers)?,
        Scratch::from_registered(rx, 32, &buffers)?,
    );
    let mut cell = UdpCell::<2, 1>::spawn(state)?;
    let mut ip_ring = IoRing::<IpOp, Result<IpDone, IpError>, 2>::new();
    let (mut ip_client, _) = ip_ring.split();
    let mut udp_ring = IoRing::<UdpOp, Result<UdpDone, UdpError>, 2>::new();
    let (mut client, mut driver) = udp_ring.split();
    client.try_submit(Submission::new(RequestId::new(1), UdpOp::Bind { port: 7 })).unwrap();
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    let Some(Ok(UdpDone::Bound(stale))) = client.try_completion().map(|done| done.into_result())
    else {
        panic!("UDP bind did not return a socket");
    };

    cell.restart()?;
    client.try_submit(Submission::new(RequestId::new(2), UdpOp::Bind { port: 7 })).unwrap();
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    let Some(Ok(UdpDone::Bound(fresh))) = client.try_completion().map(|done| done.into_result())
    else {
        panic!("UDP rebind did not return a socket");
    };

    client.try_submit(Submission::new(RequestId::new(3), UdpOp::Close(stale))).unwrap();
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);

    assert!(matches!(
        client.try_completion().map(|done| done.into_result()),
        Some(Err(UdpError::Capability(_)))
    ));
    client.try_submit(Submission::new(RequestId::new(4), UdpOp::Close(fresh))).unwrap();
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    assert_eq!(client.try_completion().map(|done| done.into_result()), Some(Ok(UdpDone::Closed)));
    Ok(())
}

#[test]
fn restart_retains_ip_lease() -> Result<(), UdpError> {
    let mut source = *b"ping";
    let mut tx = [0u8; 32];
    let mut rx = [0u8; 32];
    let mut buffers = BufferRegistry::<3>::new();
    let source = buffers.register_read(OWNER, &mut source).unwrap();
    let tx = buffers.register_read_write(OWNER, &mut tx).unwrap();
    let rx = buffers.register_read_write(OWNER, &mut rx).unwrap();
    let state = UdpState::new(
        IpAddr::V4(LOCAL_IP),
        Scratch::from_registered(tx, 32, &buffers)?,
        Scratch::from_registered(rx, 32, &buffers)?,
    );
    let mut cell = UdpCell::<1, 1>::spawn(state)?;
    let mut ip_ring = IoRing::<IpOp, Result<IpDone, IpError>, 2>::new();
    let (mut ip_client, mut ip_driver) = ip_ring.split();
    let mut udp_ring = IoRing::<UdpOp, Result<UdpDone, UdpError>, 2>::new();
    let (mut client, mut driver) = udp_ring.split();
    let config = Config::new(LOCAL_MAC, IpAddr::V4(LOCAL_IP), 24, IpAddr::V4(PEER_IP));
    let mut ip = Ip::<_, 1>::new(Capture::default(), config);

    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);
    cell.restart()?;

    client.try_submit(Submission::new(RequestId::new(1), UdpOp::Bind { port: 7 })).unwrap();
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    let Some(Ok(UdpDone::Bound(socket))) = client.try_completion().map(|done| done.into_result())
    else {
        panic!("UDP bind did not return a socket");
    };
    let operation = UdpOp::Send {
        socket,
        to: Endpoint::new(IpAddr::V4(PEER_IP), 7),
        payload: BufferOperation::new(source, 0, 4),
    };
    client.try_submit(Submission::new(RequestId::new(2), operation)).unwrap();
    cell.udp().serve(OWNER, &mut driver, &mut ip_client, &mut buffers);

    assert!(client.try_completion().is_none());
    assert!(matches!(
        ip_driver.try_next().map(Submission::into_operation),
        Some(IpOp::Send { .. })
    ));
    Ok(())
}
