//! Thin wrappers over the RISC-V Supervisor Binary Interface.
//!
//! OpenSBI runs in M-mode beneath the kernel and exposes console, timer, and
//! system-reset services through the `ecall` instruction. These helpers keep
//! the register placement of the SBI calling convention in one audited place.

use core::arch::asm;

use crate::error::SbiError;

/// Legacy console extension: write one byte to the debug console.
const EXT_CONSOLE_PUTCHAR: usize = 0x01;
/// Base extension: version and extension queries.
const EXT_BASE: usize = 0x10;
/// Debug console extension (`DBCN`).
const EXT_DEBUG_CONSOLE: usize = 0x4442_434e;
/// Timer extension (`TIME`): program the next supervisor timer interrupt.
const EXT_TIMER: usize = 0x5449_4d45;
/// System-reset extension (`SRST`): shut the machine down.
const EXT_SYSTEM_RESET: usize = 0x5352_5354;

/// `sbi_probe_extension`, function 3 of the base extension.
const FID_PROBE_EXTENSION: usize = 3;
/// `sbi_debug_console_write`, function 0 of DBCN.
const FID_CONSOLE_WRITE: usize = 0;

/// The `(a0, a1)` pair every non-legacy SBI call returns.
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

/// Reports whether the SBI implementation provides `extension`.
pub fn probe(extension: usize) -> bool {
    // SAFETY: the base extension's probe takes the extension ID in a0 and
    // touches no memory.
    let probed = unsafe { call(EXT_BASE, FID_PROBE_EXTENSION, extension, 0, 0) };
    matches!(probed.into_result(), Ok(available) if available != 0)
}

/// Reports whether the debug console extension is available.
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
    // DBCN takes a *physical* base address split across two registers. The
    // kernel identity-maps everything it can address, so the pointer is already
    // the physical address; the high half is zero on this 64-bit ABI.
    let base = bytes.as_ptr() as usize;
    // SAFETY: `console_write` takes the length in a0 and the physical base
    // address in a1/a2. The slice outlives the call, and M-mode only reads it.
    let written = unsafe { call(EXT_DEBUG_CONSOLE, FID_CONSOLE_WRITE, bytes.len(), base, 0) };
    written.into_result()
}

/// Writes a single byte through the legacy console extension.
///
/// The legacy call's return value is reserved, so a dropped byte is silent.
/// This exists only as the fallback for firmware without DBCN.
pub fn console_putchar(byte: u8) {
    // SAFETY: the legacy console extension takes the byte in a0 and clobbers no memory.
    unsafe {
        call(EXT_CONSOLE_PUTCHAR, 0, byte as usize, 0, 0);
    }
}

/// Programs the next supervisor timer interrupt for absolute time `deadline`.
pub fn set_timer(deadline: u64) {
    // SAFETY: the TIME extension's `set_timer` function takes the deadline in a0.
    unsafe {
        call(EXT_TIMER, 0, deadline as usize, 0, 0);
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

/// Issues one SBI call, returning its `(error, value)` pair.
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
    // SAFETY: register placement follows the SBI calling convention. `readonly`
    // rather than `nomem`: DBCN reads a buffer this call passes by address.
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
