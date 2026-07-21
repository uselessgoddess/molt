//! Supervisor trap handling: exception path probe and one-shot timer ticks.
//!
//! A single direct-mode vector (`__molt_trap_entry`) saves the caller-saved
//! registers, calls [`molt_trap_handler`], and returns with `sret`. Two traps
//! are handled: `ebreak` (used to prove the exception path returns) and the
//! supervisor timer interrupt (counted so the executor can await a completion).
//! Anything else is fatal and reported before shutdown.

use core::arch::{asm, global_asm};
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use molt_arch::{SerialPort, SerialWriter};

use crate::{SbiSerial, csr, sbi};

/// Set by the breakpoint handler so [`verify_breakpoint`] can observe a return.
static BREAKPOINT_SEEN: AtomicBool = AtomicBool::new(false);
/// Incremented once per serviced supervisor timer interrupt.
static TICKS: AtomicU64 = AtomicU64::new(0);

global_asm!(
    r#"
.section .text
.balign 4
.global __molt_trap_entry
__molt_trap_entry:
    addi sp, sp, -128
    sd ra,   0(sp)
    sd a0,   8(sp)
    sd a1,  16(sp)
    sd a2,  24(sp)
    sd a3,  32(sp)
    sd a4,  40(sp)
    sd a5,  48(sp)
    sd a6,  56(sp)
    sd a7,  64(sp)
    sd t0,  72(sp)
    sd t1,  80(sp)
    sd t2,  88(sp)
    sd t3,  96(sp)
    sd t4, 104(sp)
    sd t5, 112(sp)
    sd t6, 120(sp)
    call molt_trap_handler
    ld ra,   0(sp)
    ld a0,   8(sp)
    ld a1,  16(sp)
    ld a2,  24(sp)
    ld a3,  32(sp)
    ld a4,  40(sp)
    ld a5,  48(sp)
    ld a6,  56(sp)
    ld a7,  64(sp)
    ld t0,  72(sp)
    ld t1,  80(sp)
    ld t2,  88(sp)
    ld t3,  96(sp)
    ld t4, 104(sp)
    ld t5, 112(sp)
    ld t6, 120(sp)
    addi sp, sp, 128
    sret
"#
);

unsafe extern "C" {
    /// The assembly trap vector defined above.
    fn __molt_trap_entry();
}

/// Installs the supervisor trap vector. Call once during platform init.
pub fn init() {
    // SAFETY: `__molt_trap_entry` is a 4-byte-aligned handler that preserves the
    // caller-saved context around the Rust handler and returns with `sret`.
    unsafe {
        csr::set_stvec(__molt_trap_entry as *const () as usize);
    }
}

/// Triggers a breakpoint and reports whether the handler returned control.
pub fn verify_breakpoint() -> bool {
    BREAKPOINT_SEEN.store(false, Ordering::Release);
    // SAFETY: the trap vector is installed and the breakpoint handler advances
    // `sepc` past this instruction so execution resumes here.
    unsafe {
        asm!("ebreak", options(nomem, nostack));
    }
    BREAKPOINT_SEEN.load(Ordering::Acquire)
}

/// Returns the number of supervisor timer interrupts serviced so far.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Acquire)
}

/// The Rust half of the trap vector.
#[unsafe(no_mangle)]
extern "C" fn molt_trap_handler() {
    let cause = csr::scause();
    if cause & csr::CAUSE_INTERRUPT != 0 {
        let code = cause & !csr::CAUSE_INTERRUPT;
        if code == csr::INTERRUPT_TIMER {
            // Disarm the one-shot before acknowledging so it cannot re-fire, then
            // record the tick the waiting executor is polling for.
            sbi::set_timer(u64::MAX);
            TICKS.fetch_add(1, Ordering::Release);
            return;
        }
        fatal("unexpected interrupt", cause);
    }

    let code = cause;
    if code == csr::EXCEPTION_BREAKPOINT {
        BREAKPOINT_SEEN.store(true, Ordering::Release);
        // Resume past the `ebreak`, which is 2 bytes when compressed and 4 otherwise.
        let sepc = csr::sepc();
        // SAFETY: `sepc` addresses the trapping instruction in mapped kernel text.
        let opcode = unsafe { core::ptr::read(sepc as *const u16) };
        let width = if opcode & 0b11 == 0b11 { 4 } else { 2 };
        // SAFETY: resuming at the instruction after `ebreak` is the defined behaviour.
        unsafe {
            csr::set_sepc(sepc + width);
        }
        return;
    }

    fatal("unexpected exception", cause);
}

/// Reports an unrecoverable trap over the SBI console and shuts the machine down.
///
/// The report goes through [`SbiSerial`] rather than a raw `console_putchar`
/// loop, so a firmware that advertises DBCN gets the reliable, error-checked
/// path; the legacy call remains as `SbiSerial`'s automatic fallback.
fn fatal(kind: &str, cause: usize) -> ! {
    let mut serial = SbiSerial::new();
    serial.init();
    // Formatting through `SerialWriter` cannot fail on `SbiSerial`, but a full
    // trap report is worth having even if a single write returns short.
    let _ =
        writeln!(SerialWriter::new(&mut serial), "MOLT_EXCEPTION: {kind} scause=0x{cause:016x}");
    sbi::shutdown(false)
}
