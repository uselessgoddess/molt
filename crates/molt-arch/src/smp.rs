//! Starting the other cores, and reaching one afterwards.
//!
//! Bringing up a core is the least portable thing a kernel does: x86_64 pokes
//! an interrupt controller with a hand-written real-mode trampoline, RISC-V
//! asks firmware in one call. What both come down to is the same three
//! questions — how many are there, run this on that one, and get its attention
//! — so that is the whole of [`Smp`].
//!
//! Cores share nothing, so the doorbell matters as much as the start. A core
//! with no work [`park`](Smp::park)s instead of spinning, and the core that
//! hands it work [`wake`](Smp::wake)s it. That pair is what lets an idle
//! machine be idle.

use core::ptr::NonNull;

use molt_core::cpu::CpuId;

/// Why an SMP request refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmpError {
    /// This platform runs on the core firmware started, and only that one.
    Unsupported,
    /// No core is numbered that.
    Unknown,
    /// That core is already running.
    Running,
    /// Firmware or hardware refused to start it.
    Refused,
}

/// What a core runs once it is up. It never returns; the core is its own now.
pub type Entry = fn(CpuId) -> !;

/// The memory a core runs on, handed over for the life of that core.
#[derive(Clone, Copy, Debug)]
pub struct Stack {
    base: NonNull<u8>,
    bytes: usize,
}

// SAFETY: a stack is a span nobody else touches once it is handed over, so the
// pointer inside crosses to the core that will run on it.
unsafe impl Send for Stack {}

impl Stack {
    /// Names `bytes` of memory starting at `base` as one core's stack.
    ///
    /// # Safety
    ///
    /// The span must be writable, unaliased, and live as long as the core it is
    /// given to — which is until the machine stops.
    pub const unsafe fn new(base: NonNull<u8>, bytes: usize) -> Self {
        Self { base, bytes }
    }

    /// Where the core's stack pointer starts, aligned down as every ABI wants.
    pub fn top(&self) -> *mut u8 {
        let top = self.base.as_ptr() as usize + self.bytes;
        (top & !0xf) as *mut u8
    }

    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Writes the identifiers of `listed` in the order molt will number them.
///
/// Machines name their cores as firmware pleases — sparse APIC IDs, hart IDs
/// starting anywhere — and molt numbers them densely from the core it is on.
/// So the boot core comes first whatever the list said, repeats are dropped
/// (firmware may list one twice), and anything past `into` is left out, which
/// is how a machine with more cores than blocks stops at the blocks.
///
/// Returns how many were numbered.
pub fn number<T: Copy + PartialEq>(listed: &[T], boot: T, into: &mut [T]) -> usize {
    let mut count = 0;
    for id in [boot].iter().chain(listed).copied() {
        if count == into.len() || into[..count].contains(&id) {
            continue;
        }
        into[count] = id;
        count += 1;
    }
    count
}

/// The cores of the machine, from the one already running.
pub trait Smp {
    /// How many cores firmware described, this one included.
    ///
    /// Cores are numbered `0..cpus()`, which is molt's numbering rather than
    /// the machine's: see [`CpuId`].
    fn cpus(&self) -> u16 {
        1
    }

    /// Which core is asking.
    fn cpu(&self) -> CpuId {
        CpuId::BOOT
    }

    /// Starts `cpu` on `stack` and enters `entry` there.
    ///
    /// The new core comes up on the caller's page tables with traps of its own
    /// and nothing else; everything past that is `entry`'s.
    ///
    /// # Safety
    ///
    /// `stack` must be this core's alone, and `entry` must be prepared to run
    /// concurrently with everything the caller is doing.
    unsafe fn start(&mut self, _cpu: CpuId, _stack: Stack, _entry: Entry) -> Result<(), SmpError> {
        Err(SmpError::Unsupported)
    }

    /// Rings `cpu`'s doorbell, whether or not it is parked.
    ///
    /// The interrupt carries nothing. What the woken core should look at is
    /// already in its inbox — this only says to look.
    fn wake(&self, _cpu: CpuId) -> Result<(), SmpError> {
        Err(SmpError::Unsupported)
    }

    /// Stops this core until an interrupt arrives, then returns.
    ///
    /// A spurious return is normal: the caller re-checks its work and parks
    /// again. What is not allowed is sleeping through a [`wake`](Self::wake)
    /// that raced the park, so an implementation must arm before it halts.
    fn park(&self) {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::number;

    #[test]
    fn boot_numbered_first_and_once() {
        let mut ids = [0; 4];

        assert_eq!(number(&[0, 4, 2], 4, &mut ids), 3, "the boot core was numbered twice");

        assert_eq!(ids[..3], [4, 0, 2]);
    }

    #[test]
    fn cores_past_the_blocks_dropped() {
        let mut ids = [0; 2];

        assert_eq!(number(&[1, 2, 3], 0, &mut ids), 2);
    }
}
