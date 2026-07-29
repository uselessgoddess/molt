use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::cell::Cell;
use molt_core::ring::{IoDriver, IoRing, RequestId, Submission};
use molt_net::addr::{IpAddr, Ipv4Addr, MacAddr};
use molt_net::{Config, Link, LinkError};
use molt_tcp::{Endpoint, SocketStorage, Tcp, TcpCell, TcpDone, TcpError, TcpOp, TcpState};

const SERVER: CellId = CellId::new(1);
const CLIENT: CellId = CellId::new(2);
const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 1);
const CLIENT_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const PORT: u16 = 4242;

type Queue = Rc<RefCell<VecDeque<Vec<u8>>>>;
type Ring = IoRing<TcpOp, Result<TcpDone, TcpError>, 4>;
type Driver<'a> = IoDriver<'a, TcpOp, Result<TcpDone, TcpError>, 4>;

/// One end of a two-queue cable.
struct Wire {
    tx: Queue,
    rx: Queue,
}

impl Link for Wire {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.tx.borrow_mut().push_back(frame.to_vec());
        Ok(())
    }

    fn receive(&mut self, frame: &mut [u8]) -> Result<Option<usize>, LinkError> {
        let Some(next) = self.rx.borrow_mut().pop_front() else {
            return Ok(None);
        };
        frame.get_mut(..next.len()).ok_or(LinkError::Device)?.copy_from_slice(&next);
        Ok(Some(next.len()))
    }
}

fn config(mac: u8, addr: Ipv4Addr, gateway: Ipv4Addr) -> Config {
    Config::new(MacAddr::new([0x02, 0, 0, 0, 0, mac]), IpAddr::V4(addr), 24, IpAddr::V4(gateway))
}

/// Runs both ends until the cable is quiet, which a handshake needs several of.
fn spin<const M: usize>(
    now: &mut u64,
    server: &mut Tcp<'_, Wire, 1>,
    from_server: &mut Driver<'_>,
    client: &mut Tcp<'_, Wire, 1>,
    from_client: &mut Driver<'_>,
    buffers: &mut BufferRegistry<'_, M>,
) {
    for _ in 0..16 {
        *now += 10;
        server.serve(SERVER, *now, from_server, buffers);
        client.serve(CLIENT, *now, from_client, buffers);
    }
}

#[test]
fn stream_crosses_rings() -> Result<(), TcpError> {
    let (to_server, to_client) = (Queue::default(), Queue::default());
    let mut server_slots = [SocketStorage::EMPTY; 1];
    let mut client_slots = [SocketStorage::EMPTY; 1];
    let mut server_rings = [0u8; 2048];
    let mut client_rings = [0u8; 2048];
    let mut server = Tcp::<_, 1>::new(
        Wire { tx: to_client.clone(), rx: to_server.clone() },
        config(1, SERVER_IP, CLIENT_IP),
        &mut server_slots,
        &mut server_rings,
    )?;
    let mut client = Tcp::<_, 1>::new(
        Wire { tx: to_server, rx: to_client },
        config(2, CLIENT_IP, SERVER_IP),
        &mut client_slots,
        &mut client_rings,
    )?;

    let mut ping = *b"ping";
    let mut pong = *b"pong";
    let mut taken = [0u8; 8];
    let mut echoed = [0u8; 8];
    let mut buffers = BufferRegistry::<4>::new();
    let ping = buffers.register_read(CLIENT, &mut ping).unwrap();
    let pong = buffers.register_read(SERVER, &mut pong).unwrap();
    let taken = buffers.register_write(SERVER, &mut taken).unwrap();
    let echoed = buffers.register_write(CLIENT, &mut echoed).unwrap();
    let mut server_ring = Ring::new();
    let (mut listener, mut from_server) = server_ring.split();
    let mut client_ring = Ring::new();
    let (mut dialer, mut from_client) = client_ring.split();
    let mut now = 0;

    listener.try_submit(Submission::new(RequestId::new(1), TcpOp::Listen { port: PORT })).unwrap();
    let to = Endpoint::new(IpAddr::V4(SERVER_IP), PORT);
    dialer.try_submit(Submission::new(RequestId::new(1), TcpOp::Connect { to })).unwrap();
    spin(&mut now, &mut server, &mut from_server, &mut client, &mut from_client, &mut buffers);
    let Some(Ok(TcpDone::Opened(accepted))) = listener.try_completion().map(|d| d.into_result())
    else {
        panic!("listen did not return a socket");
    };
    let Some(Ok(TcpDone::Opened(dialed))) = dialer.try_completion().map(|d| d.into_result()) else {
        panic!("connect did not return a socket");
    };

    let send = TcpOp::Send { socket: dialed, payload: BufferOperation::new(ping, 0, 4) };
    dialer.try_submit(Submission::new(RequestId::new(2), send)).unwrap();
    let recv = TcpOp::Recv { socket: accepted, payload: BufferOperation::new(taken, 0, 8) };
    listener.try_submit(Submission::new(RequestId::new(2), recv)).unwrap();
    spin(&mut now, &mut server, &mut from_server, &mut client, &mut from_client, &mut buffers);

    assert_eq!(dialer.try_completion().map(|d| d.into_result()), Some(Ok(TcpDone::Sent(4))));
    assert_eq!(listener.try_completion().map(|d| d.into_result()), Some(Ok(TcpDone::Received(4))));
    assert_eq!(&buffers.resolve_write(BufferOperation::new(taken, 0, 4))?, b"ping");

    let send = TcpOp::Send { socket: accepted, payload: BufferOperation::new(pong, 0, 4) };
    listener.try_submit(Submission::new(RequestId::new(3), send)).unwrap();
    let recv = TcpOp::Recv { socket: dialed, payload: BufferOperation::new(echoed, 0, 8) };
    dialer.try_submit(Submission::new(RequestId::new(3), recv)).unwrap();
    spin(&mut now, &mut server, &mut from_server, &mut client, &mut from_client, &mut buffers);

    assert_eq!(listener.try_completion().map(|d| d.into_result()), Some(Ok(TcpDone::Sent(4))));
    assert_eq!(dialer.try_completion().map(|d| d.into_result()), Some(Ok(TcpDone::Received(4))));
    assert_eq!(&buffers.resolve_write(BufferOperation::new(echoed, 0, 4))?, b"pong");

    dialer.try_submit(Submission::new(RequestId::new(4), TcpOp::Close(dialed))).unwrap();
    let recv = TcpOp::Recv { socket: accepted, payload: BufferOperation::new(taken, 0, 8) };
    listener.try_submit(Submission::new(RequestId::new(4), recv)).unwrap();
    spin(&mut now, &mut server, &mut from_server, &mut client, &mut from_client, &mut buffers);

    assert_eq!(dialer.try_completion().map(|d| d.into_result()), Some(Ok(TcpDone::Closed)));
    // A stream nobody is writing to reads as done rather than as empty.
    assert_eq!(listener.try_completion().map(|d| d.into_result()), Some(Ok(TcpDone::Received(0))));
    Ok(())
}

#[test]
fn restart_stales_listening_socket() -> Result<(), TcpError> {
    let queue = Queue::default();
    let mut slots = [SocketStorage::EMPTY; 1];
    let mut rings = [0u8; 1024];
    let mut buffers = BufferRegistry::<1>::new();
    let state = TcpState::new(
        Wire { tx: queue.clone(), rx: queue },
        config(1, SERVER_IP, CLIENT_IP),
        &mut slots,
        &mut rings,
    );
    let mut cell = TcpCell::<_, 1>::spawn(state)?;
    let mut ring = Ring::new();
    let (mut client, mut driver) = ring.split();

    client.try_submit(Submission::new(RequestId::new(1), TcpOp::Listen { port: PORT })).unwrap();
    cell.tcp().serve(SERVER, 0, &mut driver, &mut buffers);
    let Some(Ok(TcpDone::Opened(stale))) = client.try_completion().map(|d| d.into_result()) else {
        panic!("listen did not return a socket");
    };

    cell.restart()?;
    client.try_submit(Submission::new(RequestId::new(2), TcpOp::Close(stale))).unwrap();
    cell.tcp().serve(SERVER, 10, &mut driver, &mut buffers);

    assert!(matches!(
        client.try_completion().map(|d| d.into_result()),
        Some(Err(TcpError::Capability(_)))
    ));
    client.try_submit(Submission::new(RequestId::new(3), TcpOp::Listen { port: PORT })).unwrap();
    cell.tcp().serve(SERVER, 20, &mut driver, &mut buffers);
    assert!(matches!(
        client.try_completion().map(|d| d.into_result()),
        Some(Ok(TcpDone::Opened(_)))
    ));
    Ok(())
}
