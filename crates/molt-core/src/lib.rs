// loom requires `std`; production builds remain `no_std`.
#![cfg_attr(not(loom), no_std)]
// Same wall as `molt-abi`, for the same reason one level in: these are the
// primitives the kernel runs on, and a panic in one of them is the machine
// stopping with no caller left to handle it.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Architecture-independent primitives for the Molt kernel.
//!
//! The lock-free primitives take their atomics and cells from `limen`, which
//! substitutes loom's instrumented ones under `--cfg loom` while production
//! builds remain `no_std`.

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod probe;

pub mod audit;
pub mod buffer;
pub mod cache;
pub mod capability;
pub mod cell;
pub mod completion;
pub mod cpu;
pub mod executor;
pub mod interrupt;
pub mod lock;
pub mod peers;
pub mod registry;
pub mod ring;
pub mod task;
pub mod waker;

pub use cell::CellId;
pub use cpu::CpuId;
