//! The operations carried by an IP service ring.

use molt_core::buffer::BufferOperation;
use molt_core::capability::{Capability, CapabilityRights, Read, Rights, Write};

use crate::IpAddr;

/// Authority to send and receive one IP protocol number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {}

impl CapabilityRights for Protocol {
    const MASK: Rights = Rights::READ_WRITE;
}

/// One operation submitted by a transport cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpOp {
    Bind { protocol: u8 },
    Send { endpoint: Capability<Protocol>, to: IpAddr, payload: BufferOperation<Read> },
    Recv { endpoint: Capability<Protocol>, payload: BufferOperation<Write> },
    Close(Capability<Protocol>),
}

/// What an IP operation produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpDone {
    Bound(Capability<Protocol>),
    Sent(usize),
    Received { from: IpAddr, len: usize },
    Closed,
}
