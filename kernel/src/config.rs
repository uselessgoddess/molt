//! What the kernel spends on metadata, decided in one place.
//!
//! Every number here is the length of a static array: the kernel that would
//! size these from the RAM the firmware reported is the kernel that needs the
//! address space they describe, so they are fixed before boot rather than
//! computed during it. Fixed is not the same as arbitrary — each is a budget
//! with a cost, [`bytes`](KernelConfig::bytes) adds the costs up, and running
//! out of one is an `Err` at the call site rather than a corrupted table.
//!
//! They live together because that is the only way the total is visible. Spread
//! one per module, the question "what does a booted molt spend on bookkeeping"
//! has no answer short of reading every module, and each number drifts to
//! whatever the module that owns it needed last.

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
/// Small on purpose. These are the sizes a machine with no domains running
/// still pays for, and every one of them is a table the kernel walks: a longer
/// hole list is a longer `O(holes)` search under the space lock, not just more
/// memory. They grow when something is measured to need them.
pub const CONFIG: KernelConfig = KernelConfig { holes: 64, runs: 16, windows: 4, slots: 4 };

/// Free ranges across every class, which is the array [`crate::space`] holds.
pub const HOLES: usize = CONFIG.holes * Class::ALL.len();

/// The ceiling the budget is kept under, which is what makes it a budget: raise
/// a number above and the kernel does not build until somebody raises this too.
/// Eight kibibytes is a rounding error against the RAM a machine boots with and
/// a lot of records against how few things are running.
const CEILING: usize = 8 * 1024;

const _: () = assert!(CONFIG.bytes() <= CEILING, "the kernel's metadata outgrew its budget");
