//! Architecture-neutral state carried across domain entries.

use crate::View;

/// A resumable hardware-domain context.
///
/// Register files stay inside the port: exposing x86 register names to RISC-V
/// kernel code would make the common boundary a union of both machines. The
/// portable state is the initial entry, stack, bootstrap arguments, and the
/// view both ports switch to.
pub struct DomainState {
    view: View,
    entry: u64,
    stack: u64,
    arguments: [u64; 6],
    started: bool,
}

impl DomainState {
    pub const fn new(view: View, entry: u64, stack: u64, arguments: [u64; 6]) -> Self {
        Self { view, entry, stack, arguments, started: false }
    }

    pub const fn view(&self) -> View {
        self.view
    }

    pub const fn entry(&self) -> u64 {
        self.entry
    }

    pub const fn stack(&self) -> u64 {
        self.stack
    }

    pub const fn arguments(&self) -> [u64; 6] {
        self.arguments
    }

    pub const fn started(&self) -> bool {
        self.started
    }

    pub fn resume(&mut self) {
        self.started = true;
    }
}

/// Why a protected domain returned to its supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainExit {
    /// The producer published work and asks the kernel to drive its ring.
    Ring,
    /// Normal program termination.
    Exited(i64),
    /// A processor exception contained to this view.
    Fault { cause: u64, address: u64 },
}
