//! Allocation-free Ethernet, ARP, and IPv4 wire primitives.

#![no_std]

pub mod address;
pub mod arp;
pub mod checksum;
pub mod ethernet;
pub mod ipv4;
mod link;
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
