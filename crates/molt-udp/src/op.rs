//! The operations carried by a UDP service ring.

use molt_core::buffer::BufferOperation;
use molt_core::capability::{Capability, CapabilityRights, Read, Rights, Write};

use crate::Endpoint;

/// Authority to use one bound UDP port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Socket {}

impl CapabilityRights for Socket {
    const MASK: Rights = Rights::READ_WRITE;
}

/// One operation submitted to a UDP cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UdpOp {
    Bind { port: u16 },
    Send { socket: Capability<Socket>, to: Endpoint, payload: BufferOperation<Read> },
    Recv { socket: Capability<Socket>, payload: BufferOperation<Write> },
    Close(Capability<Socket>),
}

/// What a UDP operation produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UdpDone {
    Bound(Capability<Socket>),
    Sent(usize),
    Received { from: Endpoint, len: usize },
    Closed,
}
