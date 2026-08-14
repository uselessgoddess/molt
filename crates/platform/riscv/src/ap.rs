//! Bringing a hart from firmware's hands to Rust.
//!
//! Nothing like the x86_64 dance next door: firmware starts the hart in
//! supervisor mode at an address molt names, and the kernel is identity
//! mapped, so that address is the same before and after translation. All the
//! shim below does is give the hart a stack and the boot hart's tables — the
//! two things `hart_start` has no way to pass.

use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{Ordering, fence};

use molt_arch::{CpuId, Entry, SmpError, Stack};

use crate::error::SbiError;
use crate::{csr, paging, percpu, sbi, trap};

/// How long the boot hart waits, in spins, for a hart to answer.
const SPINS: u32 = 1 << 22;

unsafe extern "C" {
    /// The shim's first instruction, which is all firmware is told.
    static __molt_ap_start: u8;
}

global_asm!(
    r#"
.pushsection .text, "ax"
.globl __molt_ap_start
__molt_ap_start:
    // a0 is the hart firmware started, a1 the block the boot hart wrote.
    ld sp, 8(a1)
    ld t0, 0(a1)
    csrw satp, t0
    sfence.vma
    ld t0, 16(a1)
    jr t0
.popsection
"#
);

/// What the shim reads, in the order it reads it.
#[repr(C)]
#[allow(dead_code, reason = "the shim reads what Rust only writes")]
struct Params {
    satp: u64,
    stack: u64,
    /// [`enter`], because the shim has no way to name a Rust function.
    enter: unsafe extern "C" fn(u64, *const Params) -> !,
    cpu: u64,
    /// What the kernel wanted run on this hart.
    hand: Entry,
}

/// One block, because harts are started one at a time and the next start waits
/// for this one to have copied it.
struct Pad(UnsafeCell<[u64; 5]>);

// SAFETY: written by the boot hart while no other hart is reading it — a start
// is not issued until the last one reported itself up.
unsafe impl Sync for Pad {}

static PAD: Pad = Pad(UnsafeCell::new([0; 5]));

/// Starts `cpu` on `stack` and enters `entry` there.
///
/// # Safety
///
/// `stack` must be nothing else's, and `entry` must be ready to run beside
/// everything the caller is doing.
pub unsafe fn start(cpu: CpuId, stack: Stack, entry: Entry) -> Result<(), SmpError> {
    let target = percpu::of(cpu).ok_or(SmpError::Unknown)?;
    if target.up() {
        return Err(SmpError::Running);
    }
    if !sbi::has_hart_state() {
        return Err(SmpError::Unsupported);
    }
    let satp = paging::satp().ok_or(SmpError::Unsupported)?;

    let pad = PAD.0.get().cast::<Params>();
    // SAFETY: the last hart started copied the block out before reporting up,
    // so nothing is reading it now.
    unsafe {
        pad.write(Params {
            satp,
            stack: stack.top() as u64,
            enter,
            cpu: u64::from(cpu.get()),
            hand: entry,
        });
    }
    // The hart reads the block with no idea a write ever happened to it.
    fence(Ordering::SeqCst);

    let shim = (&raw const __molt_ap_start) as usize;
    sbi::hart_start(target.hart(), shim, pad as usize).map_err(refused)?;
    settle(|| target.up()).then_some(()).ok_or(SmpError::Refused)
}

/// What firmware's answer means to a caller that asked for a core.
fn refused(error: SbiError) -> SmpError {
    match error {
        SbiError::InvalidParam => SmpError::Unknown,
        SbiError::AlreadyAvailable | SbiError::AlreadyStarted => SmpError::Running,
        SbiError::NotSupported => SmpError::Unsupported,
        _ => SmpError::Refused,
    }
}

/// Spins until `ready`, and reports whether it ever was.
fn settle(ready: impl Fn() -> bool) -> bool {
    (0..SPINS).any(|_| {
        spin_loop();
        ready()
    })
}

/// Where a hart arrives: molt's address space live, and nothing else set up.
///
/// # Safety
///
/// Called by the shim, on the hart firmware started, with `params` addressing
/// the block written for it.
unsafe extern "C" fn enter(hart: u64, params: *const Params) -> ! {
    // Copied out first: the block is the next hart's the moment `attach` below
    // reports this one up.
    // SAFETY: the shim passes back the block the boot hart wrote for it.
    let params = unsafe { params.read() };
    let cpu = CpuId::new(params.cpu as u16);
    // SAFETY: this is that hart, and this is its only call.
    unsafe {
        percpu::attach(cpu, hart);
        trap::init();
        csr::enable_software_interrupts();
    }
    (params.hand)(cpu)
}
