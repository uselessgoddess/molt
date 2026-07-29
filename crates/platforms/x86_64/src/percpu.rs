//! What a core keeps behind `gs`, and how it finds it again.
//!
//! The blocks are one static array rather than an allocation: this crate runs
//! before the heap exists and has to work without one. A core's `gs` base is
//! the address of its element, and the element's first word points back at
//! itself, so reading `gs:[0]` costs one load instead of an `rdmsr`.
//!
//! Only the doorbell state is the platform's own. Everything a kernel wants
//! per-core hangs off [`block`](Percpu::block), which this crate never reads.

use core::arch::asm;
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU16, AtomicU64, Ordering};

use molt_arch::CpuId;

use crate::apic;

/// Cores molt will bring up, however many the machine has.
pub const MAX: usize = 8;

const IA32_GS_BASE: u32 = 0xc000_0101;

/// One core's own memory.
#[repr(C, align(128))]
pub struct Percpu {
    /// Where this element is, read through `gs` and nothing else.
    own: AtomicPtr<Percpu>,
    /// What the kernel installed, or null.
    block: AtomicPtr<()>,
    /// This core's dense index.
    cpu: AtomicU16,
    /// The local APIC identifier the machine calls it by.
    apic: AtomicU8,
    /// A wake that arrived before the core parked.
    doorbell: AtomicBool,
    /// Set by the core itself, in [`attach`]: nothing else can tell.
    up: AtomicBool,
    /// Timer interrupts taken here.
    ticks: AtomicU64,
}

const _: () = assert!(offset_of!(Percpu, own) == 0, "`gs:[0]` must be the self pointer");

impl Percpu {
    const fn new() -> Self {
        Self {
            own: AtomicPtr::new(ptr::null_mut()),
            block: AtomicPtr::new(ptr::null_mut()),
            cpu: AtomicU16::new(0),
            apic: AtomicU8::new(0),
            doorbell: AtomicBool::new(false),
            up: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
        }
    }

    pub fn cpu(&self) -> CpuId {
        CpuId::new(self.cpu.load(Ordering::Relaxed))
    }

    pub fn apic(&self) -> u8 {
        self.apic.load(Ordering::Relaxed)
    }

    /// Whether the core reached [`attach`], which is how a start is answered.
    pub fn up(&self) -> bool {
        self.up.load(Ordering::Acquire)
    }

    pub fn block(&self) -> *mut () {
        self.block.load(Ordering::Acquire)
    }

    pub fn set_block(&self, block: *mut ()) {
        self.block.store(block, Ordering::Release);
    }

    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Acquire)
    }

    pub fn tick(&self) {
        self.ticks.fetch_add(1, Ordering::Release);
    }

    /// Records a wake, and reports whether the core was still to be told.
    pub fn ring(&self) -> bool {
        !self.doorbell.swap(true, Ordering::Release)
    }

    /// Takes a wake that arrived, if one did.
    pub fn answered(&self) -> bool {
        self.doorbell.swap(false, Ordering::Acquire)
    }
}

static CPUS: [Percpu; MAX] = [const { Percpu::new() }; MAX];

/// How many cores [`declare`] found, before which the boot core is all there is.
static COUNT: AtomicU16 = AtomicU16::new(1);

/// This core's block.
///
/// Valid from [`attach`] onwards, which is the first thing a core does.
pub fn this() -> &'static Percpu {
    let own: *const Percpu;
    // SAFETY: `attach` pointed `gs` at an element of `CPUS` and stored that
    // same address in its first word; the array is static, so both outlive us.
    unsafe {
        asm!("mov {}, gs:[0]", out(reg) own, options(nostack, readonly, preserves_flags));
        &*own
    }
}

/// Claims `cpu`'s block for the core that is running, and points `gs` at it.
///
/// # Safety
///
/// Must run on the core numbered `cpu`, and once only. Two cores sharing a
/// block would share everything hanging off it.
pub unsafe fn attach(cpu: CpuId) {
    let Some(slot) = CPUS.get(cpu.index()) else {
        return;
    };
    slot.own.store(ptr::from_ref(slot).cast_mut(), Ordering::Relaxed);
    slot.cpu.store(cpu.get(), Ordering::Relaxed);
    // SAFETY: IA32_GS_BASE takes any canonical address; this one is a static.
    unsafe { apic::write_msr(IA32_GS_BASE, ptr::from_ref(slot) as u64) };
    // Last, so that whoever started this core sees a block already filled in.
    slot.up.store(true, Ordering::Release);
}

/// Numbers the cores ACPI listed, boot core first.
///
/// Returns how many molt will use, which is fewer than were found when the
/// machine has more cores than [`MAX`] blocks.
pub fn declare(listed: &[u8], boot: u8) -> u16 {
    let mut ids = [0; MAX];
    let count = molt_arch::number(listed, boot, &mut ids);
    for (slot, &apic) in CPUS.iter().zip(&ids[..count]) {
        slot.apic.store(apic, Ordering::Relaxed);
    }
    COUNT.store(count as u16, Ordering::Release);
    count as u16
}

/// How many cores are numbered.
pub fn count() -> u16 {
    COUNT.load(Ordering::Acquire)
}

/// A numbered core's block, this one's included.
///
/// Reaching into another core's block is for the doorbell and for bringing it
/// up; everything else a core keeps is its own.
pub fn of(cpu: CpuId) -> Option<&'static Percpu> {
    (cpu.get() < count()).then(|| &CPUS[cpu.index()])
}
