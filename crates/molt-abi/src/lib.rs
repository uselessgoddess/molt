#![no_std]
// Every path here parses words a domain wrote, so a panic is a denial of
// service the domain gets to choose. Tests are exempt.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! The layouts and rules that cross the boundary to something untrusted.
//!
//! `molt_core::ring` is a ring between two ends that trust each other. The end
//! on the other side of this boundary may be hostile, may be merely broken, and
//! either way writes the shared memory whenever it likes, so the rules in
//! `docs/threat-model.md` live here as types of their own: the consumer's index
//! is kernel-private, the producer's is validated, the payload is copied before
//! it is parsed, and the one range check on the fast path is made on the copy
//! and masked.

pub mod nospec;
pub mod ring;
pub mod wire;

pub use crate::ring::{
    Channel, Completions, Domain, Fault, Hostile, Next, Peer, Reader, Submissions, Trusted,
};
pub use crate::wire::{Call, Handle, Op, Region, Reject, Reply, SLOT_BYTES, SLOT_WORDS};
