//! The filesystem under a name another cell can ask for.
//!
//! A root handle has to come from somewhere, and until now it came from init
//! reaching into the mounted volume and handing one to the shell it started.
//! That works for one client and stops working for two: every new cell means
//! another wire in init, and a cell that restarts has to be handed its
//! authority again by the code that started it.
//!
//! [`Storage`] is the name that replaces the wire. The filesystem publishes a
//! [`Mount`] under it, a client acquires a lease on the scheme, and the lease
//! is an ordinary capability — when the service restarts, the publication is
//! withdrawn and every lease on it goes stale, which is how a client finds out
//! that the root it was using belongs to an epoch that ended.

use molt_core::capability::{Capability, CapabilityRights, Rights};
use molt_core::registry::Scheme;

use crate::op::Dir;

/// What acquiring storage hands a client.
///
/// The root belongs to the service, not to the client that acquires it: a
/// client losing everything it opened is a restart, and losing the mount
/// everybody shares would be a service outage caused by one of its users.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mount {
    root: Capability<Dir>,
    checkpoint: u64,
}

impl Mount {
    pub const fn new(root: Capability<Dir>, checkpoint: u64) -> Self {
        Self { root, checkpoint }
    }

    /// The directory every path in this mount is reached from.
    pub const fn root(self) -> Capability<Dir> {
        self.root
    }

    /// The durable generation the volume carried when it was published.
    pub const fn checkpoint(self) -> u64 {
        self.checkpoint
    }
}

/// The scheme a filesystem answers for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Storage {}

impl CapabilityRights for Storage {
    const MASK: Rights = Rights::READ_WRITE;
}

impl Scheme for Storage {
    type Endpoint = Mount;

    const NAME: &'static str = "storage";
}
