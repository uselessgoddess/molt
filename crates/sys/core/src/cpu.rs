//! Which core, as a dense index.
//!
//! The number is molt's, not the machine's. APIC IDs are sparse and hart IDs
//! start wherever the firmware likes, so a platform maps whatever it was handed
//! onto this at start and nothing above ever learns which one it was. Zero is
//! the core the firmware started, which is the only one anything can assume.

/// A core, counted from the boot core at zero.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct CpuId(u16);

impl CpuId {
    /// The core firmware handed the kernel.
    pub const BOOT: Self = Self(0);

    pub const fn new(index: u16) -> Self {
        Self(index)
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn is_boot(self) -> bool {
        self.0 == 0
    }
}

impl From<CpuId> for usize {
    fn from(cpu: CpuId) -> Self {
        cpu.index()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::CpuId;

    #[test]
    fn boot_is_zero() {
        assert!(CpuId::BOOT.is_boot());
        assert!(!CpuId::new(1).is_boot());
    }
}
