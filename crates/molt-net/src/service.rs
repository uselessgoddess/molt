//! A capability-addressed IP service over an Ethernet link.

use molt_core::buffer::{BufferError, BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, CapabilityError, CapabilityTable, CellId, Write};
use molt_core::ring::{Completion, IoDriver, RequestId, Submission};

use crate::NetError;
use crate::addr::{IpAddr, Ipv4Addr, Ipv6Addr, MacAddr, link_local, solicited_node};
use crate::arp::{Operation as ArpOperation, Packet as Arp};
use crate::ethernet::{EtherType, Frame};
use crate::icmpv6::{self, Message};
use crate::ipv4::Packet as Ipv4;
use crate::ipv6::Packet as Ipv6;
use crate::link::{Link, LinkError};
use crate::neighbor::Cache;
use crate::op::{IpDone, IpOp, Protocol};

const FRAME: usize = 1514;
const IP_PACKET: usize = 1500;

/// Every host on the link (RFC 4291 §2.7.1).
const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

/// Static addressing for one IP link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    mac: MacAddr,
    addr: IpAddr,
    prefix: u8,
    gateway: IpAddr,
}

impl Config {
    pub const fn new(mac: MacAddr, addr: IpAddr, prefix: u8, gateway: IpAddr) -> Self {
        Self { mac, addr, prefix, gateway }
    }

    pub const fn mac(self) -> MacAddr {
        self.mac
    }

    pub const fn addr(self) -> IpAddr {
        self.addr
    }

    pub const fn prefix(self) -> u8 {
        self.prefix
    }

    pub const fn gateway(self) -> IpAddr {
        self.gateway
    }

    /// The address this host answers on before anything configures it.
    pub const fn link_local(self) -> Ipv6Addr {
        link_local(self.mac)
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
    id: RequestId,
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

/// An IP endpoint table and frame path owned by one network cell.
///
/// The family is whichever one [`Config`] carries: v4 resolves neighbours with
/// ARP, v6 with the two discovery messages of ICMPv6. Everything above the
/// resolver — binding a protocol, waiting on a receive, retrying a send that
/// stalled on an unknown neighbour — is the same code for both.
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
        // Groups pass the link filter and are sorted out by address: discovery
        // arrives at a solicited-node group no device filter knows about.
        if frame.dst() != self.config.mac && !frame.dst().is_multicast() {
            return Ok(false);
        }
        match frame.ether_type() {
            EtherType::Arp => self.ingest_arp(frame.payload(), driver, buffers),
            EtherType::Ipv4 => self.ingest_ipv4(frame.payload(), driver, buffers),
            EtherType::Ipv6 => self.ingest_ipv6(frame.src(), frame.payload(), driver, buffers),
        }
    }

    fn wait(
        &mut self,
        id: RequestId,
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
        let next = self.next_hop(to)?;
        let Some(hardware) = self.resolve(next) else {
            return match self.discover(next) {
                Ok(()) => Ok(SendState::Waiting),
                Err(LinkError::Busy) => Ok(SendState::Retry),
                Err(LinkError::Device) => Err(IpError::Link),
            };
        };
        let mut packet = [0u8; IP_PACKET];
        let (ether_type, len) = match (self.config.addr, to) {
            (IpAddr::V4(src), IpAddr::V4(to)) => {
                if payload.len() > IP_PACKET - Ipv4::HEADER {
                    return Err(IpError::TooLarge);
                }
                (EtherType::Ipv4, Ipv4::new(src, to, protocol, payload).emit(&mut packet)?)
            }
            (IpAddr::V6(src), IpAddr::V6(to)) => {
                if payload.len() > IP_PACKET - Ipv6::HEADER {
                    return Err(IpError::TooLarge);
                }
                (EtherType::Ipv6, Ipv6::new(src, to, protocol, payload).emit(&mut packet)?)
            }
            _ => return Err(IpError::Unsupported),
        };
        match self.transmit(hardware, ether_type, &packet[..len]) {
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

    /// Restarts a send that was parked on a neighbour now in the cache.
    fn resume<const M: usize, const R: usize>(
        &mut self,
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &BufferRegistry<'_, M>,
    ) {
        let Some(pending) = self.send else {
            return;
        };
        let IpOp::Send { to, .. } = *pending.submission.operation() else {
            return;
        };
        if self.next_hop(to).ok().and_then(|next| self.resolve(next)).is_some() {
            self.send = Some(PendingSend { waiting: false, ..pending });
            self.retry(driver, buffers);
        }
    }

    fn ingest_arp<const M: usize, const R: usize>(
        &mut self,
        bytes: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let packet = Arp::parse(bytes)?;
        self.neighbors.learn(IpAddr::V4(packet.tx_protocol()), packet.tx_hardware());
        if packet.operation() == ArpOperation::Request
            && IpAddr::V4(packet.rx_protocol()) == self.config.addr
        {
            self.arp_reply(packet.tx_hardware(), packet.tx_protocol())
                .map_err(|_| IpError::Link)?;
        }
        self.resume(driver, buffers);
        Ok(true)
    }

    fn ingest_ipv4<const M: usize, const R: usize>(
        &mut self,
        bytes: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let packet = Ipv4::parse(bytes)?;
        if IpAddr::V4(packet.dst()) != self.config.addr {
            return Ok(false);
        }
        self.deliver(IpAddr::V4(packet.src()), packet.protocol(), packet.payload(), driver, buffers)
    }

    fn ingest_ipv6<const M: usize, const R: usize>(
        &mut self,
        hardware: MacAddr,
        bytes: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let packet = Ipv6::parse(bytes)?;
        if packet.next_header() == icmpv6::PROTOCOL {
            return self.ingest_icmpv6(hardware, &packet, driver, buffers);
        }
        if !self.accepts(packet.dst()) {
            return Ok(false);
        }
        self.deliver(
            IpAddr::V6(packet.src()),
            packet.next_header(),
            packet.payload(),
            driver,
            buffers,
        )
    }

    fn ingest_icmpv6<const M: usize, const R: usize>(
        &mut self,
        hardware: MacAddr,
        packet: &Ipv6<'_>,
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let message = Message::parse(packet.src(), packet.dst(), packet.payload())?;
        // Discovery is only believed at full hop limit, which no router can
        // forge: anything less crossed a link and is not a neighbour.
        let neighborly = packet.hop_limit() == icmpv6::HOPS;
        match message {
            Message::Solicitation { target, source } if neighborly => {
                if !self.owns(target) {
                    return Ok(false);
                }
                // A duplicate-address probe has no source to answer, so the
                // advertisement goes to every host instead.
                let (dst, to) = if packet.src().is_unspecified() {
                    (MacAddr::multicast(ALL_NODES), ALL_NODES)
                } else {
                    let link = source.unwrap_or(hardware);
                    self.neighbors.learn(IpAddr::V6(packet.src()), link);
                    (link, packet.src())
                };
                self.advertise(dst, target, to).map_err(|_| IpError::Link)?;
                self.resume(driver, buffers);
            }
            Message::Advertisement { target, hardware: link, .. } if neighborly => {
                self.neighbors.learn(IpAddr::V6(target), link.unwrap_or(hardware));
                self.resume(driver, buffers);
            }
            Message::EchoRequest { id, seq, data } if self.accepts(packet.dst()) => {
                let reply = Message::EchoReply { id, seq, data };
                self.icmpv6(hardware, self.ipv6_addr(), packet.src(), reply)
                    .map_err(|_| IpError::Link)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Hands a protocol payload to whichever receive is waiting for it.
    fn deliver<const M: usize, const R: usize>(
        &mut self,
        from: IpAddr,
        protocol: u8,
        payload: &[u8],
        driver: &mut IoDriver<'_, IpOp, Result<IpDone, IpError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<bool, IpError> {
        let Some(index) = self.receives.iter().position(|slot| {
            slot.is_some_and(|wait| {
                self.protocols.get(wait.endpoint).copied().ok() == Some(protocol)
            })
        }) else {
            return Ok(false);
        };
        let wait = self.receives[index].take().unwrap();
        let result = if payload.len() > wait.payload.len() {
            Err(IpError::TooLarge)
        } else {
            let target = buffers.resolve_write(wait.payload)?;
            target[..payload.len()].copy_from_slice(payload);
            Ok(IpDone::Received { from, len: payload.len() })
        };
        self.publish(driver, Completion::new(wait.id, result));
        Ok(true)
    }

    fn arp_request(&mut self, target: Ipv4Addr) -> Result<(), LinkError> {
        let source = self.ipv4_addr().map_err(|_| LinkError::Device)?;
        let packet =
            Arp::new(ArpOperation::Request, self.config.mac, source, MacAddr::new([0; 6]), target);
        self.arp(MacAddr::BROADCAST, packet)
    }

    fn arp_reply(&mut self, hardware: MacAddr, target: Ipv4Addr) -> Result<(), LinkError> {
        let source = self.ipv4_addr().map_err(|_| LinkError::Device)?;
        let packet = Arp::new(ArpOperation::Reply, self.config.mac, source, hardware, target);
        self.arp(hardware, packet)
    }

    fn arp(&mut self, dst: MacAddr, packet: Arp) -> Result<(), LinkError> {
        let mut bytes = [0u8; Arp::LEN];
        packet.emit(&mut bytes).map_err(|_| LinkError::Device)?;
        self.transmit(dst, EtherType::Arp, &bytes)
    }

    /// Asks the link which station answers for `target`.
    fn solicit(&mut self, target: Ipv6Addr) -> Result<(), LinkError> {
        let group = solicited_node(target);
        let message = Message::Solicitation { target, source: Some(self.config.mac) };
        self.icmpv6(MacAddr::multicast(group), self.ipv6_addr(), group, message)
    }

    /// Answers that this host owns `target`.
    fn advertise(&mut self, dst: MacAddr, target: Ipv6Addr, to: Ipv6Addr) -> Result<(), LinkError> {
        let message = Message::Advertisement {
            target,
            hardware: Some(self.config.mac),
            solicited: to != ALL_NODES,
        };
        self.icmpv6(dst, target, to, message)
    }

    fn icmpv6(
        &mut self,
        dst: MacAddr,
        src: Ipv6Addr,
        to: Ipv6Addr,
        message: Message<'_>,
    ) -> Result<(), LinkError> {
        let mut payload = [0u8; IP_PACKET - Ipv6::HEADER];
        let len = message.emit(src, to, &mut payload).map_err(|_| LinkError::Device)?;
        let packet = Ipv6::new(src, to, icmpv6::PROTOCOL, &payload[..len]);
        // Discovery goes out at the hop limit it is only trusted at; echo is
        // ordinary traffic and takes the ordinary one.
        let packet = match message {
            Message::Solicitation { .. } | Message::Advertisement { .. } => {
                packet.hops(icmpv6::HOPS)
            }
            _ => packet,
        };
        let mut bytes = [0u8; IP_PACKET];
        let len = packet.emit(&mut bytes).map_err(|_| LinkError::Device)?;
        self.transmit(dst, EtherType::Ipv6, &bytes[..len])
    }

    fn transmit(
        &mut self,
        dst: MacAddr,
        ether_type: EtherType,
        payload: &[u8],
    ) -> Result<(), LinkError> {
        let mut frame = [0u8; FRAME];
        let len = Frame::new(dst, self.config.mac, ether_type, payload)
            .emit(&mut frame)
            .map_err(|_| LinkError::Device)?;
        self.link.transmit(&frame[..len])
    }

    /// Sends whatever discovery the next hop's family calls for.
    fn discover(&mut self, next: IpAddr) -> Result<(), LinkError> {
        match next {
            IpAddr::V4(next) => self.arp_request(next),
            IpAddr::V6(next) => self.solicit(next),
        }
    }

    /// The link address of a next hop, which a group already answers for.
    fn resolve(&self, next: IpAddr) -> Option<MacAddr> {
        match next {
            IpAddr::V6(group) if group.is_multicast() => Some(MacAddr::multicast(group)),
            next => self.neighbors.resolve(next),
        }
    }

    /// Whether this host answers discovery for `addr` as one of its own.
    fn owns(&self, addr: Ipv6Addr) -> bool {
        self.config.addr == IpAddr::V6(addr) || self.config.link_local() == addr
    }

    /// Whether a packet addressed to `dst` is this host's to receive.
    fn accepts(&self, dst: Ipv6Addr) -> bool {
        self.owns(dst)
            || dst == ALL_NODES
            || dst == solicited_node(self.config.link_local())
            || matches!(self.config.addr, IpAddr::V6(addr) if dst == solicited_node(addr))
    }

    fn ipv4_addr(&self) -> Result<Ipv4Addr, IpError> {
        match self.config.addr {
            IpAddr::V4(addr) => Ok(addr),
            IpAddr::V6(_) => Err(IpError::Unsupported),
        }
    }

    /// The address v6 traffic leaves this host under, which every MAC has one
    /// of whether or not anything configured a routable one.
    fn ipv6_addr(&self) -> Ipv6Addr {
        match self.config.addr {
            IpAddr::V6(addr) => addr,
            IpAddr::V4(_) => self.config.link_local(),
        }
    }

    fn next_hop(&self, dst: IpAddr) -> Result<IpAddr, IpError> {
        let on_link = match (self.config.addr, dst) {
            (IpAddr::V4(src), IpAddr::V4(dst)) => {
                same_prefix(&src.octets(), &dst.octets(), self.config.prefix.min(32))
            }
            (IpAddr::V6(src), IpAddr::V6(dst)) => {
                dst.is_multicast()
                    || link_scoped(dst)
                    || same_prefix(&src.octets(), &dst.octets(), self.config.prefix.min(128))
            }
            _ => return Err(IpError::Unsupported),
        };
        if on_link {
            return Ok(dst);
        }
        match (self.config.addr, self.config.gateway) {
            (IpAddr::V4(_), gateway @ IpAddr::V4(_)) => Ok(gateway),
            (IpAddr::V6(_), gateway @ IpAddr::V6(_)) => Ok(gateway),
            _ => Err(IpError::Unsupported),
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

/// Whether two addresses of one family agree on their first `prefix` bits.
fn same_prefix(left: &[u8], right: &[u8], prefix: u8) -> bool {
    let whole = prefix as usize / 8;
    let bits = prefix % 8;
    if left[..whole] != right[..whole] {
        return false;
    }
    if bits == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - bits);
    left[whole] & mask == right[whole] & mask
}

/// Whether an address never leaves the link it was formed on.
fn link_scoped(addr: Ipv6Addr) -> bool {
    addr.segments()[0] & 0xffc0 == 0xfe80
}
