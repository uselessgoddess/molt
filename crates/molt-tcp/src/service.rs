//! A capability-addressed TCP service over an Ethernet link.

use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, CapabilityTable, CellId, Read, Write};
use molt_core::ring::{Completion, IoDriver, RequestId};
use molt_net::addr::IpAddr;
use molt_net::{Config, Link};
use smoltcp::iface::{Config as Interfaced, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::socket::tcp::{Socket as Stream, SocketBuffer, State};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, IpEndpoint};

use crate::device::Device;
use crate::op::{Socket, TcpDone, TcpOp};
use crate::{Endpoint, TcpError};

/// The first port a connect takes for itself.
const EPHEMERAL: u16 = 49152;

/// What a parked request is waiting for.
#[derive(Clone, Copy)]
enum Work {
    Connect,
    Send(BufferOperation<Read>),
    Recv(BufferOperation<Write>),
}

#[derive(Clone, Copy)]
struct Parked {
    id: RequestId,
    socket: Capability<Socket>,
    work: Work,
}

/// N smoltcp streams behind capabilities, one parked request each.
///
/// A stream serves one request at a time: the ring is where concurrency lives,
/// and a client that wants to send while receiving takes two of them.
pub struct Tcp<'a, L, const N: usize> {
    device: Device<L>,
    iface: Interface,
    streams: SocketSet<'a>,
    handles: [SocketHandle; N],
    held: [Option<Capability<Socket>>; N],
    sockets: CapabilityTable<usize, N>,
    parked: [Option<Parked>; N],
    port: u16,
    completion: Option<Completion<Result<TcpDone, TcpError>>>,
}

impl<'a, L: Link, const N: usize> Tcp<'a, L, N> {
    /// Takes one slot and an equal share of `rings` for each of N streams.
    pub fn new(
        link: L,
        config: Config,
        slots: &'a mut [SocketStorage<'a>],
        rings: &'a mut [u8],
    ) -> Result<Self, TcpError> {
        let share = rings.len() / (2 * N);
        if slots.len() < N || share == 0 {
            return Err(TcpError::Full);
        }
        let mut device = Device::new(link);
        let mac = EthernetAddress(config.mac().octets());
        let mut iface =
            Interface::new(Interfaced::new(mac.into()), &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(config.addr().into(), config.prefix()));
        });
        match config.gateway() {
            IpAddr::V4(gateway) => {
                let _ = iface.routes_mut().add_default_ipv4_route(gateway);
            }
            IpAddr::V6(gateway) => {
                let _ = iface.routes_mut().add_default_ipv6_route(gateway);
            }
        }
        let mut streams = SocketSet::new(slots);
        let mut handles = [SocketHandle::default(); N];
        let mut rings = rings.chunks_exact_mut(share);
        for handle in &mut handles {
            let (rx, tx) = (rings.next().unwrap(), rings.next().unwrap());
            *handle = streams.add(Stream::new(SocketBuffer::new(rx), SocketBuffer::new(tx)));
        }
        Ok(Self {
            device,
            iface,
            streams,
            handles,
            held: [None; N],
            sockets: CapabilityTable::new(),
            parked: [None; N],
            port: EPHEMERAL,
            completion: None,
        })
    }

    /// Borrows the link the stack drives.
    pub const fn link(&self) -> &L {
        self.device.link()
    }

    pub(crate) fn reset(&mut self) {
        self.sockets.revoke_all();
        self.held = [None; N];
        self.parked = [None; N];
        self.completion = None;
        for &handle in &self.handles {
            self.streams.get_mut::<Stream>(handle).abort();
        }
    }

    /// Drives the stack, drains client work, and drives it once more.
    ///
    /// The second turn is what lets a send submitted against an established
    /// stream leave within the call that accepted it.
    pub fn serve<const M: usize, const U: usize>(
        &mut self,
        owner: CellId,
        now: u64,
        driver: &mut IoDriver<'_, TcpOp, Result<TcpDone, TcpError>, U>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> usize {
        let now = Instant::from_millis(now as i64);
        let mut served = self.flush(driver);
        if self.completion.is_some() {
            return served;
        }
        self.iface.poll(now, &mut self.device, &mut self.streams);
        served += self.resume(driver, buffers);
        if self.completion.is_some() {
            return served;
        }

        while let Some(submission) = driver.try_next() {
            let id = submission.id();
            let result = match submission.into_operation() {
                TcpOp::Listen { port } => {
                    self.listen(owner, port).map(|s| Some(TcpDone::Opened(s)))
                }
                TcpOp::Connect { to } => self.connect(id, owner, to),
                TcpOp::Send { socket, payload } => self.park(id, socket, Work::Send(payload)),
                TcpOp::Recv { socket, payload } => self.park(id, socket, Work::Recv(payload)),
                TcpOp::Close(socket) => self.close(socket).map(|()| Some(TcpDone::Closed)),
            };
            match result {
                Ok(None) => {}
                Ok(Some(done)) => served += self.publish(driver, Completion::new(id, Ok(done))),
                Err(error) => served += self.publish(driver, Completion::new(id, Err(error))),
            }
            if self.completion.is_some() {
                return served;
            }
        }
        self.iface.poll(now, &mut self.device, &mut self.streams);
        served + self.resume(driver, buffers)
    }

    /// Revokes every stream owned by one restarted client cell.
    pub fn revoke(&mut self, owner: CellId) -> usize {
        let revoked = self.sockets.revoke_owner(owner);
        for index in 0..N {
            if self.held[index].is_some_and(|socket| self.sockets.get(socket).is_err()) {
                self.held[index] = None;
                self.parked[index] = None;
                self.streams.get_mut::<Stream>(self.handles[index]).abort();
            }
        }
        revoked
    }

    fn listen(&mut self, owner: CellId, port: u16) -> Result<Capability<Socket>, TcpError> {
        if port == 0 {
            return Err(TcpError::Unaddressable);
        }
        let index = self.free()?;
        self.streams
            .get_mut::<Stream>(self.handles[index])
            .listen(port)
            .map_err(|_| TcpError::Busy)?;
        self.hold(owner, index)
    }

    fn connect(
        &mut self,
        id: RequestId,
        owner: CellId,
        to: Endpoint,
    ) -> Result<Option<TcpDone>, TcpError> {
        let index = self.free()?;
        let local = self.port;
        self.port = self.port.checked_add(1).unwrap_or(EPHEMERAL);
        let Self { streams, iface, handles, .. } = self;
        streams
            .get_mut::<Stream>(handles[index])
            .connect(iface.context(), IpEndpoint::new(to.addr().into(), to.port()), local)
            .map_err(|_| TcpError::Unaddressable)?;
        let socket = self.hold(owner, index)?;
        self.parked[index] = Some(Parked { id, socket, work: Work::Connect });
        Ok(None)
    }

    fn park(
        &mut self,
        id: RequestId,
        socket: Capability<Socket>,
        work: Work,
    ) -> Result<Option<TcpDone>, TcpError> {
        let index = *self.sockets.get(socket)?;
        if self.parked[index].is_some() {
            return Err(TcpError::Busy);
        }
        self.parked[index] = Some(Parked { id, socket, work });
        Ok(None)
    }

    /// Hands the capability back and starts the shutdown, which outlives it.
    fn close(&mut self, socket: Capability<Socket>) -> Result<(), TcpError> {
        let index = *self.sockets.get(socket)?;
        if self.parked[index].is_some() {
            return Err(TcpError::Busy);
        }
        self.streams.get_mut::<Stream>(self.handles[index]).close();
        self.sockets.revoke(socket)?;
        self.held[index] = None;
        Ok(())
    }

    /// A stream nobody holds, whose last connection has finished closing.
    fn free(&self) -> Result<usize, TcpError> {
        (0..N)
            .find(|&index| {
                self.held[index].is_none()
                    && self.streams.get::<Stream>(self.handles[index]).state() == State::Closed
            })
            .ok_or(TcpError::Full)
    }

    /// Publishes one stream to its owner, undoing the open if nothing can hold it.
    fn hold(&mut self, owner: CellId, index: usize) -> Result<Capability<Socket>, TcpError> {
        match self.sockets.insert::<Socket>(owner, index) {
            Ok(socket) => {
                self.held[index] = Some(socket);
                Ok(socket)
            }
            Err(_) => {
                self.streams.get_mut::<Stream>(self.handles[index]).abort();
                Err(TcpError::Full)
            }
        }
    }

    fn resume<const M: usize, const U: usize>(
        &mut self,
        driver: &mut IoDriver<'_, TcpOp, Result<TcpDone, TcpError>, U>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> usize {
        let mut served = 0;
        for index in 0..N {
            let Some(parked) = self.parked[index] else {
                continue;
            };
            let Some(done) = self.progress(index, parked.work, parked.socket, buffers) else {
                continue;
            };
            self.parked[index] = None;
            served += self.publish(driver, Completion::new(parked.id, done));
            if self.completion.is_some() {
                break;
            }
        }
        served
    }

    /// Answers a parked request, or `None` while the stream cannot serve it.
    fn progress<const M: usize>(
        &mut self,
        index: usize,
        work: Work,
        socket: Capability<Socket>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Option<Result<TcpDone, TcpError>> {
        let stream = self.streams.get_mut::<Stream>(self.handles[index]);
        match work {
            Work::Connect => match stream.state() {
                State::Closed => Some(Err(TcpError::Reset)),
                _ if stream.may_send() => Some(Ok(TcpDone::Opened(socket))),
                _ => None,
            },
            Work::Send(payload) => {
                if !stream.may_send() {
                    return Some(Err(TcpError::Reset));
                }
                if !stream.can_send() {
                    return None;
                }
                let bytes = match buffers.resolve_read(payload) {
                    Ok(bytes) => bytes,
                    Err(error) => return Some(Err(error.into())),
                };
                Some(stream.send_slice(bytes).map(TcpDone::Sent).map_err(|_| TcpError::Reset))
            }
            Work::Recv(payload) => {
                if !stream.can_recv() {
                    return match stream.state() {
                        State::Closed => Some(Err(TcpError::Reset)),
                        State::Listen | State::SynSent | State::SynReceived => None,
                        _ if stream.may_recv() => None,
                        // Nothing more will arrive: the peer is done sending.
                        _ => Some(Ok(TcpDone::Received(0))),
                    };
                }
                let target = match buffers.resolve_write(payload) {
                    Ok(target) => target,
                    Err(error) => return Some(Err(error.into())),
                };
                Some(stream.recv_slice(target).map(TcpDone::Received).map_err(|_| TcpError::Reset))
            }
        }
    }

    fn flush<const U: usize>(
        &mut self,
        driver: &mut IoDriver<'_, TcpOp, Result<TcpDone, TcpError>, U>,
    ) -> usize {
        let Some(completion) = self.completion.take() else {
            return 0;
        };
        self.publish(driver, completion)
    }

    fn publish<const U: usize>(
        &mut self,
        driver: &mut IoDriver<'_, TcpOp, Result<TcpDone, TcpError>, U>,
        completion: Completion<Result<TcpDone, TcpError>>,
    ) -> usize {
        match driver.try_complete(completion) {
            Ok(()) => 1,
            Err(completion) => {
                self.completion = Some(completion);
                0
            }
        }
    }
}
