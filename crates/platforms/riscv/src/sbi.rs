//! Thin wrappers over the RISC-V Supervisor Binary Interface.
//!
//! Console, timer, and reset calls keep their `ecall` register ABI here.

use core::arch::asm;

use crate::error::SbiError;

const EXT_CONSOLE_PUTCHAR: usize = 0x01;
const EXT_BASE: usize = 0x10;
const EXT_DEBUG_CONSOLE: usize = 0x4442_434e;
const EXT_TIMER: usize = 0x5449_4d45;
const EXT_SYSTEM_RESET: usize = 0x5352_5354;

const FID_PROBE_EXTENSION: usize = 3;
const FID_CONSOLE_WRITE: usize = 0;

struct SbiReturn {
    error: isize,
    value: usize,
}

impl SbiReturn {
    fn into_result(self) -> Result<usize, SbiError> {
        match SbiError::from_code(self.error) {
            None => Ok(self.value),
            Some(error) => Err(error),
        }
    }
}

pub fn probe(extension: usize) -> bool {
    // SAFETY: the base extension's probe takes the extension ID in a0 and
    // touches no memory.
    let probed = unsafe { call(EXT_BASE, FID_PROBE_EXTENSION, extension, 0, 0) };
    matches!(probed.into_result(), Ok(available) if available != 0)
}

pub fn has_debug_console() -> bool {
    probe(EXT_DEBUG_CONSOLE)
}

/// Writes `bytes` through the debug console extension.
///
/// Returns the number of bytes the implementation accepted, which may be fewer
/// than were offered; the caller is expected to resubmit the remainder.
pub fn debug_console_write(bytes: &[u8]) -> Result<usize, SbiError> {
    if bytes.is_empty() {
        return Ok(0);
    }
    // Identity mapping makes the slice pointer DBCN's physical base address.
    let base = bytes.as_ptr() as usize;
    // SAFETY: the live slice supplies DBCN's length and read-only physical address.
    let written = unsafe { call(EXT_DEBUG_CONSOLE, FID_CONSOLE_WRITE, bytes.len(), base, 0) };
    written.into_result()
}

/// Writes one byte through the error-blind legacy fallback.
pub fn console_putchar(byte: u8) {
    // SAFETY: the legacy console extension takes the byte in a0 and clobbers no memory.
    unsafe {
        call(EXT_CONSOLE_PUTCHAR, 0, byte as usize, 0, 0);
    }
}

pub fn set_timer(deadline: u64) {
    // SAFETY: the TIME extension's `set_timer` function takes the deadline in a0.
    unsafe {
        call(EXT_TIMER, 0, deadline as usize, 0, 0);
    }
}

pub fn shutdown(success: bool) -> ! {
    const RESET_TYPE_SHUTDOWN: usize = 0x0000_0000;
    const REASON_NONE: usize = 0x0000_0000;
    const REASON_SYSFAIL: usize = 0x0000_0001;
    let reason = if success { REASON_NONE } else { REASON_SYSFAIL };
    // SAFETY: the SRST extension takes the reset type in a0 and reason in a1.
    unsafe {
        call(EXT_SYSTEM_RESET, 0, RESET_TYPE_SHUTDOWN, reason, 0);
    }
    // A conforming SBI never returns from a shutdown; park the hart if it does.
    loop {
        // SAFETY: the hart has no remaining work after a failed reset request.
        unsafe {
            asm!("wfi", options(nomem, nostack));
        }
    }
}

/// Issues one SBI call.
///
/// # Safety
///
/// The caller must pass an extension/function pair whose argument registers
/// follow the SBI calling convention for that call, and must keep any memory
/// an argument points at valid for the duration of the call.
unsafe fn call(
    extension: usize,
    function: usize,
    arg0: usize,
    arg1: usize,
    arg2: usize,
) -> SbiReturn {
    let error: isize;
    let value: usize;
    // SAFETY: registers follow the SBI ABI; `readonly` permits DBCN buffer reads.
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arg0 as isize => error,
            inlateout("a1") arg1 => value,
            in("a2") arg2,
            in("a6") function,
            in("a7") extension,
            options(nostack, readonly),
        );
    }
    SbiReturn { error, value }
}
