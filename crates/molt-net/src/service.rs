//! A capability-addressed IPv4 service over an Ethernet link.

use molt_core::buffer::{BufferError, BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, CapabilityError, CapabilityTable, CellId, Write};
use molt_core::ring::{Completion, IoDriver, Submission};

use crate::NetError;
use crate::address::{IpAddr, Ipv4Addr, MacAddress};
use crate::arp::{Operation as ArpOperation, Packet as Arp};
use crate::ethernet::{EtherType, Frame};
use crate::ipv4::Packet as Ipv4;
use crate::link::{Link, LinkError};
use crate::neighbor::Cache;
use crate::op::{IpDone, IpOp, Protocol};

const FRAME: usize = 1514;
const IP_PACKET: usize = 1500;
const IP_PAYLOAD: usize = 1480;

/// Static addressing for one IP link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    mac: MacAddress,
    address: IpAddr,
    prefix: u8,
    gateway: IpAddr,
}

impl Config {
    pub const fn new(mac: MacAddress, address: IpAddr, prefix: u8, gateway: IpAddr) -> Self {
        Self { mac, address, prefix, gateway }
    }

    pub const fn mac(self) -> MacAddress {
        self.mac
    }

    pub const fn address(self) -> IpAddr {
        self.address
    }
}

/// Why the IP service refused an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpError {
    Wire(NetError),
    Capability(CapabilityError),
    Buffer(BufferError),
    Bound,
    Busy,
    Full,
    Link,
    TooLarge,
    Unsupported,
}

impl From<NetError> for IpError {
    fn from(error: NetError) -> Self {
        Self::Wire(error)
    }
}

impl From<CapabilityError> for IpError {
    fn from(error: CapabilityError) -> Self {
        Self::Capability(error)
    }
}

impl From<BufferError> for IpError {
    fn from(error: BufferError) -> Self {
        Self::Buffer(error)
    }
}

#[derive(Clone, Copy)]
struct Receive {
    id: molt_core::ring::RequestId,
    endpoint: Capability<Protocol>,
    payload: BufferOperation<Write>,
}

#[derive(Clone, Copy)]
struct PendingSend {
    submission: Submission<IpOp>,
    waiting: bool,
}

enum SendState {
    Sent(usize),
    Waiting,
    Retry,
}

/// An IPv4 endpoint table and frame path owned by one network cell.
pub struct Ip<L, const N: usize> {
    link: L,
    config: Config,
    protocols: CapabilityTable<u8, N>,
    bound: [Option<u8>; N],
    neighbors: Cache<N>,
    receives: [Option<Receive>; N],
    send: Option<PendingSend>,
    completion: Option<Completion<Result<IpDone, IpError>>>,
}

impl<L: Link, const N: usize> Ip<L, N> {
    pub const fn new(link: L, config: Config) -> Self {
        Self {
            link,
            config,
            protocols: CapabilityTable::new(),
            bound: [None; N],
            neighbors: Cache::new(),
            receives: [None; N],
            send: None,
            completion: None,
        }
    }

    pub const fn link(&self) -> &L {
        &self.link
    }

    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    /// Returns the stopped service's link to its device owner.
    pub fn into_link(self) -> L {
        self.link
    }

    pub fn bind(&mut self, owner: CellId, protocol: u8) -> Result<Capability<Protocol>, IpError> {
        if self.bound.contains(&Some(protocol)) {
            return Err(IpError::Bound);
        }
        let endpoint =
            self.protocols.insert::<Protocol>(owner, protocol).map_err(|_| IpError::Full)?;
        let slot = self.bound.iter_mut().find(|slot| slot.is_none()).ok_or(IpError::Full)?;
        *slot = Some(protocol);
        Ok(endpoint)
    }

    /// Drains transport-cell requests until hardware or completion backpressure stops it.
    pub fn serve<const M: usize, const R: usize>(
        &mut self,
        owner: CellId,
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> usize {
        let mut served = self.flush(driver);
        if self.completion.is_some() {
            return served;
        }
        served += self.retry(driver, buffers);
        if self.send.is_some() || self.completion.is_some() {
            return served;
        }

        while let Some(submission) = driver.try_next() {
            let id = submission.id();
            match submission.into_operation() {
                IpOp::Bind { protocol } => {
                    let result = self.bind(owner, protocol).map(IpDone::Bound);
                    served += self.publish(driver, Completion::new(id, result));
                }
                op @ IpOp::Send { .. } => {
                    let submission = Submission::new(id, op);
                    match self.try_send(op, buffers) {
                        Ok(SendState::Sent(len)) => {
                            served +=
                                self.publish(driver, Completion::new(id, Ok(IpDone::Sent(len))));
                        }
                        Ok(SendState::Waiting) => {
                            self.send = Some(PendingSend { submission, waiting: true });
                        }
                        Ok(SendState::Retry) => {
                            self.send = Some(PendingSend { submission, waiting: false });
                        }
                        Err(error) => {
                            served += self.publish(driver, Completion::new(id, Err(error)));
                        }
                    }
                }
                IpOp::Recv { endpoint, payload } => {
                    let result = self.wait(id, endpoint, payload);
                    if let Err(error) = result {
                        served += self.publish(driver, Completion::new(id, Err(error)));
                    }
                }
                IpOp::Close(endpoint) => {
                    let result = self.close(endpoint).map(|()| IpDone::Closed);
                    served += self.publish(driver, Completion::new(id, result));
                }
            }
            if self.send.is_some() || self.completion.is_some() {
                break;
            }
        }
        served
    }

    /// Routes one interrupt-delivered Ethernet frame to a waiting protocol ring.
    pub fn ingest<const M: usize, const R: usize>(
        &mut self,
        bytes: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        if self.completion.is_some() {
            return Err(IpError::Busy);
        }
        let frame = Frame::parse(bytes)?;
        if frame.destination() != self.config.mac && frame.destination() != MacAddress::BROADCAST {
            return Ok(false);
        }
        match frame.ether_type() {
            EtherType::Arp => self.ingest_arp(frame.payload(), driver, buffers),
            EtherType::Ipv4 => self.ingest_ipv4(frame.payload(), driver, buffers),
            EtherType::Ipv6 => Ok(false),
        }
    }

    fn wait(
        &mut self,
        id: molt_core::ring::RequestId,
        endpoint: Capability<Protocol>,
        payload: BufferOperation<Write>,
    ) -> Result<(), IpError> {
        let protocol = *self.protocols.get(endpoint)?;
        if self
            .receives
            .iter()
            .flatten()
            .any(|wait| self.protocols.get(wait.endpoint).copied().ok() == Some(protocol))
        {
            return Err(IpError::Busy);
        }
        let slot = self.receives.iter_mut().find(|slot| slot.is_none()).ok_or(IpError::Full)?;
        *slot = Some(Receive { id, endpoint, payload });
        Ok(())
    }

    fn close(&mut self, endpoint: Capability<Protocol>) -> Result<(), IpError> {
        if self.receives.iter().flatten().any(|wait| wait.endpoint == endpoint) {
            return Err(IpError::Busy);
        }
        let protocol = self.protocols.revoke(endpoint)?;
        if let Some(slot) = self.bound.iter_mut().find(|slot| **slot == Some(protocol)) {
            *slot = None;
        }
        Ok(())
    }

    fn try_send<const M: usize>(
        &mut self,
        op: IpOp,
        buffers: &BufferRegistry<'_, M>,
    ) -> Result<SendState, IpError> {
        let IpOp::Send { endpoint, to, payload } = op else {
            return Err(IpError::Busy);
        };
        let protocol = *self.protocols.get(endpoint)?;
        let payload = buffers.resolve_read(payload)?;
        if payload.len() > IP_PAYLOAD {
            return Err(IpError::TooLarge);
        }
        let next = self.next_hop(to)?;
        let Some(hardware) = self.neighbors.resolve(IpAddr::V4(next)) else {
            return match self.arp_request(next) {
                Ok(()) => Ok(SendState::Waiting),
                Err(LinkError::Busy) => Ok(SendState::Retry),
                Err(LinkError::Device) => Err(IpError::Link),
            };
        };
        let mut packet = [0u8; IP_PACKET];
        let source = self.ipv4_address()?;
        let IpAddr::V4(to) = to else {
            return Err(IpError::Unsupported);
        };
        let len = Ipv4::new(source, to, protocol, payload).emit(&mut packet)?;
        let mut frame = [0u8; FRAME];
        let len = Frame::new(hardware, self.config.mac, EtherType::Ipv4, &packet[..len])
            .emit(&mut frame)?;
        match self.link.transmit(&frame[..len]) {
            Ok(()) => Ok(SendState::Sent(payload.len())),
            Err(LinkError::Busy) => Ok(SendState::Retry),
            Err(LinkError::Device) => Err(IpError::Link),
        }
    }

    fn retry<const M: usize, const R: usize>(
        &mut self,
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &BufferRegistry<'_, M>,
    ) -> usize {
        let Some(pending) = self.send else {
            return 0;
        };
        if pending.waiting {
            return 0;
        }
        let id = pending.submission.id();
        match self.try_send(*pending.submission.operation(), buffers) {
            Ok(SendState::Sent(len)) => {
                self.send = None;
                self.publish(driver, Completion::new(id, Ok(IpDone::Sent(len))))
            }
            Ok(SendState::Waiting) => {
                self.send = Some(PendingSend { waiting: true, ..pending });
                0
            }
            Ok(SendState::Retry) => 0,
            Err(error) => {
                self.send = None;
                self.publish(driver, Completion::new(id, Err(error)))
            }
        }
    }

    fn ingest_arp<const M: usize, const R: usize>(
        &mut self,
        bytes: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let packet = Arp::parse(bytes)?;
        self.neighbors.learn(IpAddr::V4(packet.sender_protocol()), packet.sender_hardware());
        if packet.operation() == ArpOperation::Request
            && IpAddr::V4(packet.target_protocol()) == self.config.address
        {
            self.arp_reply(packet.sender_hardware(), packet.sender_protocol())
                .map_err(|_| IpError::Link)?;
        }
        if let Some(pending) = self.send {
            let IpOp::Send { to, .. } = *pending.submission.operation() else {
                return Err(IpError::Busy);
            };
            if self
                .next_hop(to)
                .ok()
                .and_then(|next| self.neighbors.resolve(IpAddr::V4(next)))
                .is_some()
            {
                self.send = Some(PendingSend { waiting: false, ..pending });
                self.retry(driver, buffers);
            }
        }
        Ok(true)
    }

    fn ingest_ipv4<const M: usize, const R: usize>(
        &mut self,
        bytes: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let packet = Ipv4::parse(bytes)?;
        if IpAddr::V4(packet.destination()) != self.config.address {
            return Ok(false);
        }
        let Some(index) = self.receives.iter().position(|slot| {
            slot.is_some_and(|wait| {
                self.protocols.get(wait.endpoint).copied().ok() == Some(packet.protocol())
            })
        }) else {
            return Ok(false);
        };
        let wait = self.receives[index].take().unwrap();
        let result = if packet.payload().len() > wait.payload.len() {
            Err(IpError::TooLarge)
        } else {
            let target = buffers.resolve_write(wait.payload)?;
            target[..packet.payload().len()].copy_from_slice(packet.payload());
            Ok(IpDone::Received { from: IpAddr::V4(packet.source()), len: packet.payload().len() })
        };
        self.publish(driver, Completion::new(wait.id, result));
        Ok(true)
    }

    fn arp_request(&mut self, target: Ipv4Addr) -> Result<(), LinkError> {
        let mut packet = [0u8; 28];
        Arp::new(
            ArpOperation::Request,
            self.config.mac,
            self.ipv4_address().map_err(|_| LinkError::Device)?,
            MacAddress::new([0; 6]),
            target,
        )
        .emit(&mut packet)
        .map_err(|_| LinkError::Device)?;
        let mut frame = [0u8; 64];
        let len = Frame::new(MacAddress::BROADCAST, self.config.mac, EtherType::Arp, &packet)
            .emit(&mut frame)
            .map_err(|_| LinkError::Device)?;
        self.link.transmit(&frame[..len])
    }

    fn arp_reply(&mut self, hardware: MacAddress, target: Ipv4Addr) -> Result<(), LinkError> {
        let mut packet = [0u8; 28];
        Arp::new(
            ArpOperation::Reply,
            self.config.mac,
            self.ipv4_address().map_err(|_| LinkError::Device)?,
            hardware,
            target,
        )
        .emit(&mut packet)
        .map_err(|_| LinkError::Device)?;
        let mut frame = [0u8; 64];
        let len = Frame::new(hardware, self.config.mac, EtherType::Arp, &packet)
            .emit(&mut frame)
            .map_err(|_| LinkError::Device)?;
        self.link.transmit(&frame[..len])
    }

    fn ipv4_address(&self) -> Result<Ipv4Addr, IpError> {
        match self.config.address {
            IpAddr::V4(address) => Ok(address),
            IpAddr::V6(_) => Err(IpError::Unsupported),
        }
    }

    fn next_hop(&self, destination: IpAddr) -> Result<Ipv4Addr, IpError> {
        let source = self.ipv4_address()?;
        let IpAddr::V4(destination) = destination else {
            return Err(IpError::Unsupported);
        };
        if same_network(source, destination, self.config.prefix.min(32)) {
            Ok(destination)
        } else {
            match self.config.gateway {
                IpAddr::V4(gateway) => Ok(gateway),
                IpAddr::V6(_) => Err(IpError::Unsupported),
            }
        }
    }

    fn flush<const R: usize>(
        &mut self,
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
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

    fn publish<const R: usize>(
        &mut self,
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        completion: Completion<Result<IpDone, IpError>>,
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

fn same_network(left: Ipv4Addr, right: Ipv4Addr, prefix: u8) -> bool {
    let left = u32::from_be_bytes(left.octets());
    let right = u32::from_be_bytes(right.octets());
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    left & mask == right & mask
}
