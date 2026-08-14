//! What the kernel spends on metadata, decided in one place.
//!
//! Every number is the length of a static array, fixed before boot rather than
//! sized from firmware's report: sizing them needs the address space they are
//! there to describe. Running out of one is an `Err` at the call site, never a
//! corrupted table.
//!
//! They live together because that is the only way the total
//! ([`bytes`](KernelConfig::bytes)) is visible at all. One per module, and
//! "what does a booted molt spend on bookkeeping" has no answer short of
//! reading every module.

use molt_arch::cache::Window;
use molt_arch::refcount::Run;
use molt_arch::va::{Class, Hole};

/// The metadata budget, in records.
pub struct KernelConfig {
    /// Free ranges *per class*: what the VA allocator can remember about the
    /// holes a revoke leaves behind. `docs/va-allocator.md` sizes it — the
    /// worst case is a domain that releases every other extent, and running out
    /// costs addresses, not correctness, because a hole nobody can record is
    /// one the space keeps rather than one it hands out twice.
    pub holes: usize,
    /// Leaf-count records. One run covers any number of adjacent leaves held
    /// the same number of times, so this is a bound on how *unevenly* the
    /// machine is shared, not on how much of it is.
    pub runs: usize,
    /// File-cache windows: distinct file offsets kept resident at once.
    pub windows: usize,
    /// Slots in one domain's ring, in each direction.
    pub slots: usize,
}

impl KernelConfig {
    /// What the arrays these numbers size cost, in bytes.
    pub const fn bytes(&self) -> usize {
        self.holes * Class::ALL.len() * size_of::<Hole>()
            + self.runs * size_of::<Run>()
            + self.windows * size_of::<Window>()
    }
}

/// The budget a molt kernel boots with.
///
/// Small on purpose: this is what a machine with no domains running still pays,
/// and a longer hole list is a longer `O(holes)` search under the space lock,
/// not only more memory. They grow when something is measured to need it.
pub const CONFIG: KernelConfig = KernelConfig { holes: 64, runs: 16, windows: 4, slots: 4 };

/// Free ranges across every class, which is the array [`crate::space`] holds.
pub const HOLES: usize = CONFIG.holes * Class::ALL.len();

/// The ceiling that makes the budget a budget: raise a number past it and the
/// kernel does not build until somebody raises this too. Eight kibibytes is a
/// rounding error against a machine's RAM and a lot of records against how few
/// things run.
const CEILING: usize = 8 * 1024;

const _: () = assert!(CONFIG.bytes() <= CEILING, "the kernel's metadata outgrew its budget");
