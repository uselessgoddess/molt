//! Thin wrappers over the RISC-V Supervisor Binary Interface.
//!
//! OpenSBI runs in M-mode beneath the kernel and exposes console, timer, and
//! system-reset services through the `ecall` instruction. These helpers keep
//! the register placement of the SBI calling convention in one audited place.

use core::arch::asm;

/// Legacy console extension: write one byte to the debug console.
const EXT_CONSOLE_PUTCHAR: usize = 0x01;
/// Timer extension (`TIME`): program the next supervisor timer interrupt.
const EXT_TIMER: usize = 0x5449_4d45;
/// System-reset extension (`SRST`): shut the machine down.
const EXT_SYSTEM_RESET: usize = 0x5352_5354;

/// Writes a single byte to the SBI debug console.
pub fn console_putchar(byte: u8) {
    // SAFETY: the legacy console extension takes the byte in a0 and clobbers no memory.
    unsafe {
        call(EXT_CONSOLE_PUTCHAR, 0, byte as usize, 0);
    }
}

/// Programs the next supervisor timer interrupt for absolute time `deadline`.
pub fn set_timer(deadline: u64) {
    // SAFETY: the TIME extension's `set_timer` function takes the deadline in a0.
    unsafe {
        call(EXT_TIMER, 0, deadline as usize, 0);
    }
}

/// Requests an orderly shutdown, reporting success or failure to the host.
pub fn shutdown(success: bool) -> ! {
    const RESET_TYPE_SHUTDOWN: usize = 0x0000_0000;
    const REASON_NONE: usize = 0x0000_0000;
    const REASON_SYSFAIL: usize = 0x0000_0001;
    let reason = if success { REASON_NONE } else { REASON_SYSFAIL };
    // SAFETY: the SRST extension takes the reset type in a0 and reason in a1.
    unsafe {
        call(EXT_SYSTEM_RESET, 0, RESET_TYPE_SHUTDOWN, reason);
    }
    // A conforming SBI never returns from a shutdown; park the hart if it does.
    loop {
        // SAFETY: the hart has no remaining work after a failed reset request.
        unsafe {
            asm!("wfi", options(nomem, nostack));
        }
    }
}

/// Issues one SBI call, returning the `a0` error field.
///
/// # Safety
///
/// The caller must pass an extension/function pair whose argument registers
/// follow the SBI calling convention for that call.
unsafe fn call(extension: usize, function: usize, arg0: usize, arg1: usize) -> isize {
    let error: isize;
    // SAFETY: register placement follows the SBI calling convention; `ecall` preserves memory.
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arg0 as isize => error,
            in("a1") arg1,
            in("a6") function,
            in("a7") extension,
            options(nostack),
        );
    }
    error
}
