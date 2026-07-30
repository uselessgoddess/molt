//! What a core keeps behind `tp`, and how it finds it again.
//!
//! The blocks are one static array rather than an allocation: this crate runs
//! before the heap exists and has to work without one. A core's `tp` holds the
//! address of its element directly, which is the whole trick — no self pointer
//! and no CSR read, `tp` is already a register.
//!
//! Only the doorbell state is the platform's own. Everything a kernel wants
//! per-core hangs off [`block`](Percpu::block), which this crate never reads.

use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, AtomicU64, Ordering};

use molt_arch::CpuId;

/// Cores molt will bring up, however many the machine has.
pub const MAX: usize = 8;

/// One core's own memory.
#[repr(C, align(64))]
pub struct Percpu {
    /// What the kernel installed, or null.
    block: AtomicPtr<()>,
    /// This core's dense index.
    cpu: AtomicU16,
    /// The hart identifier firmware calls it by.
    hart: AtomicU64,
    /// A wake that arrived before the core parked.
    doorbell: AtomicBool,
    /// Set by the hart itself, in [`attach`]: nothing else can tell.
    up: AtomicBool,
    /// Timer interrupts taken here.
    ticks: AtomicU64,
}

impl Percpu {
    const fn new() -> Self {
        Self {
            block: AtomicPtr::new(ptr::null_mut()),
            cpu: AtomicU16::new(0),
            hart: AtomicU64::new(0),
            doorbell: AtomicBool::new(false),
            up: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
        }
    }

    pub fn cpu(&self) -> CpuId {
        CpuId::new(self.cpu.load(Ordering::Relaxed))
    }

    pub fn hart(&self) -> u64 {
        self.hart.load(Ordering::Relaxed)
    }

    /// Whether the hart reached [`attach`], which is how a start is answered.
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
    // SAFETY: `attach` put an element of the static `CPUS` in `tp`, and nothing
    // in molt uses `tp` for anything else.
    unsafe {
        asm!("mv {}, tp", out(reg) own, options(nomem, nostack, preserves_flags));
        &*own
    }
}

/// Claims `cpu`'s block for the hart that is running, and points `tp` at it.
///
/// # Safety
///
/// Must run on `hart`, which must be the core numbered `cpu`, and once only.
/// Two cores sharing a block would share everything hanging off it.
pub unsafe fn attach(cpu: CpuId, hart: u64) {
    let Some(slot) = CPUS.get(cpu.index()) else {
        return;
    };
    slot.cpu.store(cpu.get(), Ordering::Relaxed);
    slot.hart.store(hart, Ordering::Relaxed);
    // SAFETY: `tp` is molt's to set; the address is a static that outlives us.
    unsafe { asm!("mv tp, {}", in(reg) ptr::from_ref(slot), options(nostack)) };
    // Last, so that whoever started this hart sees a block already filled in.
    slot.up.store(true, Ordering::Release);
}

/// Numbers the harts firmware listed, boot hart first.
///
/// Returns how many molt will use, which is fewer than were found when the
/// machine has more harts than [`MAX`] blocks.
pub fn declare(listed: &[u64], boot: u64) -> u16 {
    let mut ids = [0; MAX];
    let count = molt_arch::number(listed, boot, &mut ids);
    for (slot, &hart) in CPUS.iter().zip(&ids[..count]) {
        slot.hart.store(hart, Ordering::Relaxed);
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
