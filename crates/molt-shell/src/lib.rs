//! An asynchronous shell over a [`molt_fs`] ring.
//!
//! The shell is a client and nothing more. It acquires a directory capability
//! from the namespace, submits [`molt_fs::FsOp`] and awaits [`molt_fs::FsDone`],
//! and reads file bytes out of a buffer it registered — the same interface any
//! other cell would use, which is the point of having it: if `cat` needs
//! something the protocol does not offer, the protocol is wrong. Nothing hands
//! it authority, so nothing has to hand it authority again after a restart.
//!
//! Input is a script for now. No platform reads its serial port back yet, so
//! [`Shell::run`] takes a line from wherever the caller found one and an
//! interactive front-end is a line editor away.

#![no_std]

#[cfg(test)]
extern crate std;

use molt_fs::FsError;

#[cfg(test)]
mod capture;
mod console;
mod session;
mod shell;

pub use molt_core::task::{drive, drive_bounded};

pub use crate::console::Console;
pub use crate::session::Session;
pub use crate::shell::{PROMPT, Shell};

/// Why a shell stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellError {
    /// The filesystem refused an operation.
    Fs(FsError),
    /// Nothing publishes storage, so there is nothing to run a command against.
    Unavailable,
    /// An answer that does not belong to the operation that was asked.
    Protocol,
}

impl From<FsError> for ShellError {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}
