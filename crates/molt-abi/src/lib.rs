#![no_std]

//! The layouts and rules that cross the boundary to something untrusted.
//!
//! [`SpscRing`] is a fine ring between two ends that trust
//! each other, and its safety contract says so. The end on the other side of
//! this boundary is a domain that may be hostile, may be merely broken, and in
//! either case gets to write the shared memory whenever it likes. So the rules
//! in [`docs/threat-model.md`](../../../docs/threat-model.md) are implemented
//! here as a separate type: the consumer's index is kernel-private, the
//! producer's is validated, the payload is copied and then parsed, and the one
//! range check the fast path is allowed is made on the copy and masked.
//!
//! [`SpscRing`]: ../molt_core/ring/struct.SpscRing.html

pub mod ring;
pub mod wire;

pub use crate::ring::{Channel, Completions, Domain, Fault, Next, Submissions};
pub use crate::wire::{Call, Handle, Op, Region, Reject, Reply, SLOT_BYTES, SLOT_WORDS};
