// loom is a host-only test harness and pulls in `std`; every other build stays
// `no_std`.
#![cfg_attr(not(loom), no_std)]

//! Architecture-independent primitives for the Molt kernel.
//!
//! This crate deliberately stays `no_std` so its synchronization and cell
//! lifecycle rules can be tested on the host and used unchanged in the kernel.
//!
//! The lock-free primitives reach their atomics through [`sync`], which swaps
//! in loom's instrumented equivalents under `--cfg loom`. See `docs/testing.md`
//! for what that buys and where it stops short.

pub mod buffer;
pub mod cache;
pub mod capability;
pub mod cell;
pub mod completion;
pub mod executor;
pub mod ring;
pub(crate) mod sync;
pub mod waker;
