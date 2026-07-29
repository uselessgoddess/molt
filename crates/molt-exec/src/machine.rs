//! What an executor asks the machine under it for.
//!
//! Four questions, all of them a register read or an interrupt: which core is
//! this, ring that one, stop until someone rings, and what time is it. An
//! executor asks for nothing else — no memory, no mapping, no lock — which is
//! what keeps it testable on a host that has none of them.

use molt_core::cpu::CpuId;

pub trait Machine: Sync {
    /// Which core is asking.
    fn cpu(&self) -> CpuId {
        CpuId::BOOT
    }

    /// Rings `cpu`'s doorbell, parked or not.
    fn wake(&self, cpu: CpuId) {
        let _ = cpu;
    }

    /// Stops this core until an interrupt arrives.
    ///
    /// An implementation must arm before it halts: a wake that raced the park
    /// is one the core has to take, not sleep through.
    fn park(&self) {
        core::hint::spin_loop();
    }

    /// This core's tick, which is what deadlines are counted in.
    fn ticks(&self) -> u64 {
        0
    }
}

/// One core, no doorbell, and a clock the caller advances: a host test.
pub struct Solo;

impl Machine for Solo {}
