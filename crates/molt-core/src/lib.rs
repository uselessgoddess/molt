// loom requires `std`; production builds remain `no_std`.
#![cfg_attr(not(loom), no_std)]

//! Architecture-independent primitives for the Molt kernel.
//!
//! The lock-free primitives use `sync` to substitute loom's instrumented
//! atomics under `--cfg loom` while production builds remain `no_std`.

pub mod buffer;
pub mod cache;
pub mod capability;
pub mod cell;
pub mod completion;
pub mod executor;
pub mod interrupt;
pub mod ring;
pub(crate) mod sync;
pub mod waker;
