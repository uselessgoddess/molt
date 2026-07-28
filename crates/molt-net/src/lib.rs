//! Allocation-free Ethernet and IP wire primitives.

#![no_std]

pub mod addr;
pub mod arp;
pub mod checksum;
pub mod ethernet;
pub mod ipv4;
mod link;
pub mod neighbor;
mod op;
mod service;

pub use crate::link::{Link, LinkError};
pub use crate::op::{IpDone, IpOp, Protocol};
pub use crate::service::{Config, Ip, IpError};

/// Why a network frame could not be read or written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetError {
    Buffer,
    Malformed,
    Checksum,
    Fragmented,
    Unsupported,
}
