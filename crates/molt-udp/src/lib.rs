//! Allocation-free UDP datagrams and a capability-addressed service.

#![no_std]

mod cell;
mod error;
mod op;
mod service;
mod wire;

pub use crate::cell::{UdpCell, UdpState};
pub use crate::error::UdpError;
pub use crate::op::{Socket, UdpDone, UdpOp};
pub use crate::service::{Scratch, Udp};
pub use crate::wire::{Datagram, Endpoint, PROTOCOL};
