//! Minimal supervisor control and status register access.

use core::arch::asm;

/// `sstatus.SIE` — global supervisor interrupt enable.
pub const SSTATUS_SIE: usize = 1 << 1;
/// `sie.STIE` — supervisor timer interrupt enable.
pub const SIE_STIE: usize = 1 << 5;

/// Trap cause bit set when the trap is an interrupt rather than an exception.
pub const CAUSE_INTERRUPT: usize = 1 << (usize::BITS as usize - 1);
/// Exception code for a supervisor timer interrupt.
pub const INTERRUPT_TIMER: usize = 5;
/// Exception code for an executed `ebreak`.
pub const EXCEPTION_BREAKPOINT: usize = 3;

macro_rules! read_csr {
    ($name:ident) => {{
        let value: usize;
        // SAFETY: reading a supervisor CSR has no side effects and clobbers no memory.
        unsafe {
            asm!(concat!("csrr {0}, ", stringify!($name)), out(reg) value, options(nomem, nostack));
        }
        value
    }};
}

macro_rules! write_csr {
    ($name:ident, $value:expr) => {{
        let value: usize = $value;
        // SAFETY: the caller of the wrapping function guarantees the write is valid.
        unsafe {
            asm!(concat!("csrw ", stringify!($name), ", {0}"), in(reg) value, options(nomem, nostack));
        }
    }};
}

pub fn scause() -> usize {
    read_csr!(scause)
}

pub fn sepc() -> usize {
    read_csr!(sepc)
}

/// Reads the `time` counter (monotonic real time in timebase ticks).
pub fn time() -> u64 {
    read_csr!(time) as u64
}

/// # Safety
///
/// `value` must be a valid resumption address for the interrupted context.
pub unsafe fn set_sepc(value: usize) {
    write_csr!(sepc, value);
}

/// Installs the direct-mode trap vector.
///
/// # Safety
///
/// `base` must point to a 4-byte-aligned trap entry that preserves and restores
/// the interrupted context.
pub unsafe fn set_stvec(base: usize) {
    // Mode bits [1:0] = 0 selects direct mode: every trap enters `base`.
    write_csr!(stvec, base & !0b11);
}

/// # Safety
///
/// A valid trap vector must be installed before timer interrupts are enabled.
pub unsafe fn enable_timer_interrupts() {
    // SAFETY: set only the timer-enable bits, leaving other interrupt sources as configured.
    unsafe {
        asm!(
            "csrs sie, {stie}",
            "csrs sstatus, {sie}",
            stie = in(reg) SIE_STIE,
            sie = in(reg) SSTATUS_SIE,
            options(nomem, nostack),
        );
    }
}
