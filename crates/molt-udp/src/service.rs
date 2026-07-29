//! A capability-addressed UDP service over an IP ring.

use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, CapabilityTable, CellId, Read, ReadWrite, Write};
use molt_core::ring::{Completion, IoClient, IoDriver, RequestId, Submission};
use molt_net::addr::IpAddr;
use molt_net::{IpDone, IpError, IpOp, Protocol};

use crate::op::{Socket, UdpDone, UdpOp};
use crate::{Datagram, Endpoint, UdpError};

const BIND_ID: RequestId = RequestId::new(0);
const RECV_ID: RequestId = RequestId::new(1);
const SEND_ID: RequestId = RequestId::new(2);
const MAX_PAYLOAD: usize = 1472;
const MAX_SEGMENT: usize = 1480;

/// Read and write views of one service-owned registered buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scratch {
    read: BufferOperation<Read>,
    write: BufferOperation<Write>,
}

impl Scratch {
    /// Pairs read and write capabilities for the same service-owned buffer.
    pub const fn new(read: BufferOperation<Read>, write: BufferOperation<Write>) -> Self {
        Self { read, write }
    }

    /// Derives bounded views from one registered read-write buffer.
    pub fn from_registered<const N: usize>(
        buffer: Capability<ReadWrite>,
        len: usize,
        buffers: &BufferRegistry<'_, N>,
    ) -> Result<Self, UdpError> {
        Ok(Self::new(
            BufferOperation::new(buffers.read_capability(buffer)?, 0, len),
            BufferOperation::new(buffers.write_capability(buffer)?, 0, len),
        ))
    }
}

#[derive(Clone, Copy)]
struct Bound {
    port: u16,
    socket: Capability<Socket>,
}

#[derive(Clone, Copy)]
struct Receive {
    id: RequestId,
    socket: Capability<Socket>,
    payload: BufferOperation<Write>,
}

#[derive(Clone, Copy)]
struct Send {
    id: RequestId,
    socket: Capability<Socket>,
    len: usize,
}

#[derive(Clone, Copy)]
struct Stored {
    socket: Capability<Socket>,
    from: Endpoint,
    len: usize,
    bytes: [u8; MAX_PAYLOAD],
}

/// A UDP socket table and pair of IP-ring scratch buffers.
pub struct Udp<const N: usize, const Q: usize> {
    local: IpAddr,
    tx: Scratch,
    rx: Scratch,
    sockets: CapabilityTable<u16, N>,
    bound: [Option<Bound>; N],
    receives: [Option<Receive>; N],
    inbox: [Option<Stored>; Q],
    endpoint: Option<Capability<Protocol>>,
    bind_inflight: bool,
    recv_inflight: bool,
    send_inflight: bool,
    send: Option<Send>,
    completion: Option<Completion<Result<UdpDone, UdpError>>>,
}

impl<const N: usize, const Q: usize> Udp<N, Q> {
    /// Creates an empty socket table over two service-owned scratch buffers.
    pub const fn new(local: IpAddr, tx: Scratch, rx: Scratch) -> Self {
        Self {
            local,
            tx,
            rx,
            sockets: CapabilityTable::new(),
            bound: [None; N],
            receives: [None; N],
            inbox: [None; Q],
            endpoint: None,
            bind_inflight: false,
            recv_inflight: false,
            send_inflight: false,
            send: None,
            completion: None,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.sockets.revoke_all();
        self.bound = [None; N];
        self.receives = [None; N];
        self.inbox = [None; Q];
        // Lower-ring leases and accepted requests survive the upper service epoch.
        self.send = None;
        self.completion = None;
    }

    /// Drains client work and advances the lower IP ring without polling a device.
    pub fn serve<const M: usize, const U: usize, const I: usize>(
        &mut self,
        owner: CellId,
        driver: &mut IoDriver<'_, UdpOp, Result<UdpDone, UdpError>, U>,
        ip: &mut IoClient<'_, IpOp, Result<IpDone, IpError>, I>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> usize {
        let mut served = self.flush(driver);
        if self.completion.is_some() {
            return served;
        }
        served += self.pump_ip(driver, ip, buffers);
        if self.completion.is_some() {
            return served;
        }

        while let Some(submission) = driver.try_next() {
            let id = submission.id();
            match submission.into_operation() {
                UdpOp::Bind { port } => {
                    let result = self.bind(owner, port).map(UdpDone::Bound);
                    served += self.publish(driver, Completion::new(id, result));
                }
                UdpOp::Send { socket, to, payload } => {
                    match self.start_send(id, socket, to, payload, ip, buffers) {
                        Ok(None) => {}
                        Ok(Some(done)) => {
                            served += self.publish(driver, Completion::new(id, Ok(done)));
                        }
                        Err(error) => {
                            served += self.publish(driver, Completion::new(id, Err(error)));
                        }
                    }
                }
                UdpOp::Recv { socket, payload } => {
                    match self.start_recv(id, socket, payload, buffers) {
                        Ok(None) => {}
                        Ok(Some(done)) => {
                            served += self.publish(driver, Completion::new(id, Ok(done)));
                        }
                        Err(error) => {
                            served += self.publish(driver, Completion::new(id, Err(error)));
                        }
                    }
                }
                UdpOp::Close(socket) => {
                    let result = self.close(socket).map(|()| UdpDone::Closed);
                    served += self.publish(driver, Completion::new(id, result));
                }
            }
            if self.completion.is_some() {
                break;
            }
        }
        self.arm_ip(ip);
        served
    }

    /// Revokes all sockets owned by one restarted client cell.
    pub fn revoke(&mut self, owner: CellId) -> usize {
        let revoked = self.sockets.revoke_owner(owner);
        for slot in &mut self.bound {
            if slot.is_some_and(|bound| self.sockets.get(bound.socket).is_err()) {
                *slot = None;
            }
        }
        for slot in &mut self.receives {
            if slot.is_some_and(|receive| self.sockets.get(receive.socket).is_err()) {
                *slot = None;
            }
        }
        for slot in &mut self.inbox {
            if slot.is_some_and(|stored| self.sockets.get(stored.socket).is_err()) {
                *slot = None;
            }
        }
        if self.send.is_some_and(|send| self.sockets.get(send.socket).is_err()) {
            self.send = None;
        }
        revoked
    }

    fn bind(&mut self, owner: CellId, port: u16) -> Result<Capability<Socket>, UdpError> {
        if port == 0 || self.bound.iter().flatten().any(|bound| bound.port == port) {
            return Err(UdpError::Bound);
        }
        let socket = self.sockets.insert::<Socket>(owner, port).map_err(|_| UdpError::Full)?;
        let slot = self.bound.iter_mut().find(|slot| slot.is_none()).ok_or(UdpError::Full)?;
        *slot = Some(Bound { port, socket });
        Ok(socket)
    }

    fn start_send<const M: usize, const I: usize>(
        &mut self,
        id: RequestId,
        socket: Capability<Socket>,
        to: Endpoint,
        payload: BufferOperation<Read>,
        ip: &mut IoClient<'_, IpOp, Result<IpDone, IpError>, I>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<Option<UdpDone>, UdpError> {
        let endpoint = self.endpoint.ok_or(UdpError::Busy)?;
        if self.send_inflight {
            return Err(UdpError::Busy);
        }
        let port = *self.sockets.get(socket)?;
        let source = buffers.resolve_read(payload)?;
        if source.len() > MAX_PAYLOAD {
            return Err(UdpError::TooLarge);
        }
        let source_len = source.len();
        let mut copy = [0u8; MAX_PAYLOAD];
        copy[..source_len].copy_from_slice(source);
        let target = buffers.resolve_write(self.tx.write)?;
        let len =
            Datagram::new(Endpoint::new(self.local, port), to, &copy[..source_len]).emit(target)?;
        let op = IpOp::Send {
            endpoint,
            to: to.addr(),
            payload: BufferOperation::new(self.tx.read.capability(), 0, len),
        };
        if ip.try_submit(Submission::new(SEND_ID, op)).is_err() {
            return Err(UdpError::Busy);
        }
        self.send_inflight = true;
        self.send = Some(Send { id, socket, len: source_len });
        Ok(None)
    }

    fn start_recv<const M: usize>(
        &mut self,
        id: RequestId,
        socket: Capability<Socket>,
        payload: BufferOperation<Write>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<Option<UdpDone>, UdpError> {
        self.sockets.get(socket)?;
        if self.receives.iter().flatten().any(|receive| receive.socket == socket) {
            return Err(UdpError::Busy);
        }
        if let Some(index) =
            self.inbox.iter().position(|stored| stored.is_some_and(|item| item.socket == socket))
        {
            let stored = self.inbox[index].take().unwrap();
            return self.deliver(stored, Receive { id, socket, payload }, buffers).map(Some);
        }
        let slot = self.receives.iter_mut().find(|slot| slot.is_none()).ok_or(UdpError::Full)?;
        *slot = Some(Receive { id, socket, payload });
        Ok(None)
    }

    fn close(&mut self, socket: Capability<Socket>) -> Result<(), UdpError> {
        if self.receives.iter().flatten().any(|receive| receive.socket == socket)
            || self.send.is_some_and(|send| send.socket == socket)
        {
            return Err(UdpError::Busy);
        }
        self.sockets.revoke(socket)?;
        if let Some(slot) =
            self.bound.iter_mut().find(|bound| bound.is_some_and(|bound| bound.socket == socket))
        {
            *slot = None;
        }
        for slot in &mut self.inbox {
            if slot.is_some_and(|stored| stored.socket == socket) {
                *slot = None;
            }
        }
        Ok(())
    }

    fn pump_ip<const M: usize, const U: usize, const I: usize>(
        &mut self,
        driver: &mut IoDriver<'_, UdpOp, Result<UdpDone, UdpError>, U>,
        ip: &mut IoClient<'_, IpOp, Result<IpDone, IpError>, I>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> usize {
        let mut served = 0;
        while let Some(completion) = ip.try_completion() {
            match completion.id() {
                BIND_ID => {
                    self.bind_inflight = false;
                    if let Ok(IpDone::Bound(endpoint)) = completion.into_result() {
                        self.endpoint = Some(endpoint);
                    }
                }
                RECV_ID => {
                    self.recv_inflight = false;
                    if let Ok(IpDone::Received { from, len }) = completion.into_result() {
                        if let Some((id, result)) = self.receive(from, len, buffers) {
                            served += self.publish(driver, Completion::new(id, result));
                        }
                    }
                }
                SEND_ID => {
                    self.send_inflight = false;
                    let result = completion.into_result();
                    if let Some(send) = self.send.take() {
                        let result = match result {
                            Ok(IpDone::Sent(_)) => Ok(UdpDone::Sent(send.len)),
                            Ok(_) => Err(UdpError::Malformed),
                            Err(error) => Err(UdpError::Ip(error)),
                        };
                        served += self.publish(driver, Completion::new(send.id, result));
                    }
                }
                _ => {}
            }
            if self.completion.is_some() {
                break;
            }
        }
        self.arm_ip(ip);
        served
    }

    fn receive<const M: usize>(
        &mut self,
        source: IpAddr,
        len: usize,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Option<(RequestId, Result<UdpDone, UdpError>)> {
        if len > self.rx.read.len() || len > MAX_SEGMENT {
            return None;
        }
        let bytes =
            buffers.resolve_read(BufferOperation::new(self.rx.read.capability(), 0, len)).ok()?;
        let datagram = Datagram::parse(source, self.local, bytes).ok()?;
        let bound = self
            .bound
            .iter()
            .flatten()
            .find(|bound| bound.port == datagram.dst().port())
            .copied()?;
        if datagram.payload().len() > MAX_PAYLOAD {
            return None;
        }
        let mut stored = Stored {
            socket: bound.socket,
            from: datagram.src(),
            len: datagram.payload().len(),
            bytes: [0; MAX_PAYLOAD],
        };
        stored.bytes[..stored.len].copy_from_slice(datagram.payload());
        if let Some(index) = self
            .receives
            .iter()
            .position(|receive| receive.is_some_and(|receive| receive.socket == bound.socket))
        {
            let receive = self.receives[index].take().unwrap();
            let id = receive.id;
            return Some((id, self.deliver(stored, receive, buffers)));
        }
        if let Some(slot) = self.inbox.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(stored);
        }
        None
    }

    fn deliver<const M: usize>(
        &self,
        stored: Stored,
        receive: Receive,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<UdpDone, UdpError> {
        if stored.len > receive.payload.len() {
            return Err(UdpError::TooLarge);
        }
        let target = buffers.resolve_write(receive.payload)?;
        target[..stored.len].copy_from_slice(&stored.bytes[..stored.len]);
        Ok(UdpDone::Received { from: stored.from, len: stored.len })
    }

    fn arm_ip<const I: usize>(&mut self, ip: &mut IoClient<'_, IpOp, Result<IpDone, IpError>, I>) {
        if self.endpoint.is_none() && !self.bind_inflight {
            if ip
                .try_submit(Submission::new(
                    BIND_ID,
                    IpOp::Bind { protocol: crate::wire::PROTOCOL },
                ))
                .is_ok()
            {
                self.bind_inflight = true;
            }
            return;
        }
        let Some(endpoint) = self.endpoint else {
            return;
        };
        if !self.recv_inflight {
            let op = IpOp::Recv { endpoint, payload: self.rx.write };
            if ip.try_submit(Submission::new(RECV_ID, op)).is_ok() {
                self.recv_inflight = true;
            }
        }
    }

    fn flush<const U: usize>(
        &mut self,
        driver: &mut IoDriver<'_, UdpOp, Result<UdpDone, UdpError>, U>,
    ) -> usize {
        let Some(completion) = self.completion.take() else {
            return 0;
        };
        match driver.try_complete(completion) {
            Ok(()) => 1,
            Err(completion) => {
                self.completion = Some(completion);
                0
            }
        }
    }

    fn publish<const U: usize>(
        &mut self,
        driver: &mut IoDriver<'_, UdpOp, Result<UdpDone, UdpError>, U>,
        completion: Completion<Result<UdpDone, UdpError>>,
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
