//! The operations carried by a TCP service ring.

use molt_core::buffer::BufferOperation;
use molt_core::capability::{Capability, CapabilityRights, Read, Rights, Write};

use crate::Endpoint;

/// Authority to use one TCP connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Socket {}

impl CapabilityRights for Socket {
    const MASK: Rights = Rights::READ_WRITE;
}

/// One operation submitted to a TCP cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpOp {
    Listen { port: u16 },
    Connect { to: Endpoint },
    Send { socket: Capability<Socket>, payload: BufferOperation<Read> },
    Recv { socket: Capability<Socket>, payload: BufferOperation<Write> },
    Close(Capability<Socket>),
}

/// What a TCP operation produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpDone {
    /// A socket that is listening, or one whose handshake finished.
    Opened(Capability<Socket>),
    Sent(usize),
    /// Bytes taken from the stream; zero once the peer is done sending.
    Received(usize),
    Closed,
}
