#![no_std]

//! Architecture-independent primitives for the Molt kernel.
//!
//! This crate deliberately stays `no_std` so its synchronization and cell
//! lifecycle rules can be tested on the host and used unchanged in the kernel.

pub mod capability;
pub mod cell;
pub mod completion;
pub mod executor;
pub mod ring;
