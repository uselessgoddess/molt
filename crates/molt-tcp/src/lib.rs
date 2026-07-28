//! A capability-addressed TCP service backed by smoltcp.

#![no_std]

mod cell;
mod device;
mod error;
mod op;
mod service;

pub use molt_net::addr::Endpoint;
pub use smoltcp::iface::SocketStorage;

pub use crate::cell::{TcpCell, TcpState};
pub use crate::error::TcpError;
pub use crate::op::{Socket, TcpDone, TcpOp};
pub use crate::service::Tcp;
