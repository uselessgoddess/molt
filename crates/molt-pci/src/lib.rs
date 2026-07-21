#![no_std]

//! PCI configuration space as typed views over a mapped window.
//!
//! The crate never maps anything and never touches an interrupt controller. It
//! is handed a [`Config`] — a 32-bit window onto one segment's configuration
//! space — and turns the bytes behind it into addresses, bars, capabilities,
//! and MSI-X tables. Everything that needs a page table or a vector number
//! lives in a platform crate, so the whole enumeration path is testable on the
//! host against a buffer.
//!
//! The split matters for more than tests. Configuration space is device memory
//! reached through a window `Inventory::device` had to approve; if this crate
//! could produce a pointer, it could produce one the audit never saw.

pub mod address;
pub mod bar;
pub mod capability;
pub mod config;
pub mod ecam;
pub mod error;
pub mod function;
pub mod message;
pub mod msi;
pub mod msix;
pub mod scan;

pub use crate::address::Address;
pub use crate::bar::Bar;
pub use crate::capability::Capability;
pub use crate::config::Config;
pub use crate::ecam::Ecam;
pub use crate::error::Error;
pub use crate::function::{Command, Function, Id};
pub use crate::message::Message;
pub use crate::msi::Msi;
pub use crate::msix::{MsiX, Table};
pub use crate::scan::scan;
