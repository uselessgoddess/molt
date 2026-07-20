//! Thin wrappers over the RISC-V Supervisor Binary Interface.
//!
//! OpenSBI runs in M-mode beneath the kernel and exposes debug-console, timer,
//! and system-reset services through `ecall`. Console output prefers DBCN's
//! multi-byte write and falls back to the deprecated legacy byte call when the
//! extension is absent or rejects the remaining output.

#[cfg(target_arch = "riscv64")]
use core::arch::asm;

/// Base extension: discover standard SBI extensions.
const EXT_BASE: usize = 0x10;
/// Legacy console extension: write one byte.
const EXT_CONSOLE_PUTCHAR: usize = 0x01;
/// Debug Console Extension (`DBCN`).
const EXT_DEBUG_CONSOLE: usize = 0x4442_434e;
/// Timer extension (`TIME`): program the next supervisor timer interrupt.
#[cfg(target_arch = "riscv64")]
const EXT_TIMER: usize = 0x5449_4d45;
/// System-reset extension (`SRST`): shut the machine down.
#[cfg(target_arch = "riscv64")]
const EXT_SYSTEM_RESET: usize = 0x5352_5354;

const FID_PROBE_EXTENSION: usize = 3;
const FID_CONSOLE_WRITE: usize = 0;
const SBI_SUCCESS: isize = 0;

#[derive(Clone, Copy)]
struct SbiRet {
    error: isize,
    value: usize,
}

trait Backend {
    fn call(&self, extension: usize, function: usize, arguments: [usize; 3]) -> SbiRet;
}

#[cfg(target_arch = "riscv64")]
struct Ecall;

#[cfg(target_arch = "riscv64")]
impl Backend for Ecall {
    fn call(&self, extension: usize, function: usize, arguments: [usize; 3]) -> SbiRet {
        // SAFETY: every caller uses the register layout defined for its SBI function.
        unsafe { call(extension, function, arguments) }
    }
}

#[derive(Clone, Copy)]
enum ConsoleMode {
    Debug,
    Legacy,
}

/// SBI diagnostic console selected during platform initialization.
pub struct Console {
    mode: ConsoleMode,
}

impl Console {
    pub const fn new() -> Self {
        Self { mode: ConsoleMode::Legacy }
    }

    #[cfg(target_arch = "riscv64")]
    pub fn init(&mut self) {
        *self = Self::probe_with(&Ecall);
    }

    #[cfg(target_arch = "riscv64")]
    pub fn write(&self, bytes: &[u8]) {
        self.write_with(&Ecall, bytes);
    }

    fn probe_with(backend: &impl Backend) -> Self {
        let result = backend.call(EXT_BASE, FID_PROBE_EXTENSION, [EXT_DEBUG_CONSOLE, 0, 0]);
        let mode = if result.error == SBI_SUCCESS && result.value != 0 {
            ConsoleMode::Debug
        } else {
            ConsoleMode::Legacy
        };
        Self { mode }
    }

    fn write_with(&self, backend: &impl Backend, mut bytes: &[u8]) {
        if matches!(self.mode, ConsoleMode::Debug) {
            while !bytes.is_empty() {
                // Molt's Sv39 kernel mappings are identity mappings, so this
                // virtual slice address is also the physical address DBCN expects.
                let result = backend.call(
                    EXT_DEBUG_CONSOLE,
                    FID_CONSOLE_WRITE,
                    [bytes.len(), bytes.as_ptr() as usize, 0],
                );
                if result.error != SBI_SUCCESS || result.value == 0 || result.value > bytes.len() {
                    break;
                }
                bytes = &bytes[result.value..];
            }
        }

        for byte in bytes {
            backend.call(EXT_CONSOLE_PUTCHAR, 0, [usize::from(*byte), 0, 0]);
        }
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

/// Programs the next supervisor timer interrupt for absolute time `deadline`.
#[cfg(target_arch = "riscv64")]
pub fn set_timer(deadline: u64) {
    Ecall.call(EXT_TIMER, 0, [deadline as usize, 0, 0]);
}

/// Requests an orderly shutdown, reporting success or failure to the host.
#[cfg(target_arch = "riscv64")]
pub fn shutdown(success: bool) -> ! {
    const RESET_TYPE_SHUTDOWN: usize = 0x0000_0000;
    const REASON_NONE: usize = 0x0000_0000;
    const REASON_SYSFAIL: usize = 0x0000_0001;
    let reason = if success { REASON_NONE } else { REASON_SYSFAIL };
    Ecall.call(EXT_SYSTEM_RESET, 0, [RESET_TYPE_SHUTDOWN, reason, 0]);

    // A conforming SBI never returns from a shutdown; park the hart if it does.
    loop {
        // SAFETY: the hart has no remaining work after a failed reset request.
        unsafe {
            asm!("wfi", options(nomem, nostack));
        }
    }
}

/// Issues one SBI call and returns its `a0` error and `a1` value fields.
///
/// # Safety
///
/// The caller must pass an extension/function pair whose argument registers
/// follow the SBI calling convention for that call.
#[cfg(target_arch = "riscv64")]
unsafe fn call(extension: usize, function: usize, arguments: [usize; 3]) -> SbiRet {
    let error: isize;
    let value: usize;
    // SAFETY: register placement follows the SBI calling convention. No
    // `nomem` option is used because DBCN may read from the supplied buffer.
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arguments[0] as isize => error,
            inlateout("a1") arguments[1] => value,
            in("a2") arguments[2],
            in("a6") function,
            in("a7") extension,
            options(nostack),
        );
    }
    SbiRet { error, value }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::{Backend, Console, EXT_BASE, EXT_CONSOLE_PUTCHAR, EXT_DEBUG_CONSOLE, SbiRet};

    const OK: SbiRet = SbiRet { error: 0, value: 0 };

    struct Mock {
        probe: usize,
        debug_results: [SbiRet; 3],
        debug_calls: Cell<[usize; 3]>,
        debug_count: Cell<usize>,
        legacy: Cell<[u8; 8]>,
        legacy_len: Cell<usize>,
    }

    impl Mock {
        const fn new(probe: usize, debug_results: [SbiRet; 3]) -> Self {
            Self {
                probe,
                debug_results,
                debug_calls: Cell::new([0; 3]),
                debug_count: Cell::new(0),
                legacy: Cell::new([0; 8]),
                legacy_len: Cell::new(0),
            }
        }

        fn legacy(&self) -> ([u8; 8], usize) {
            (self.legacy.get(), self.legacy_len.get())
        }
    }

    impl Backend for Mock {
        fn call(&self, extension: usize, _function: usize, arguments: [usize; 3]) -> SbiRet {
            if extension == EXT_BASE {
                return SbiRet { error: 0, value: self.probe };
            }
            if extension == EXT_DEBUG_CONSOLE {
                let index = self.debug_count.get();
                let mut calls = self.debug_calls.get();
                calls[index] = arguments[0];
                self.debug_calls.set(calls);
                self.debug_count.set(index + 1);
                return self.debug_results[index];
            }
            assert_eq!(extension, EXT_CONSOLE_PUTCHAR);
            let index = self.legacy_len.get();
            let mut legacy = self.legacy.get();
            legacy[index] = arguments[0] as u8;
            self.legacy.set(legacy);
            self.legacy_len.set(index + 1);
            OK
        }
    }

    #[test]
    fn missing_dbcn_uses_legacy() {
        let backend = Mock::new(0, [OK; 3]);
        let console = Console::probe_with(&backend);

        console.write_with(&backend, b"molt");

        assert_eq!(backend.legacy(), ([b'm', b'o', b'l', b't', 0, 0, 0, 0], 4));
        assert_eq!(backend.debug_count.get(), 0);
    }

    #[test]
    fn partial_dbcn_write_retries_remainder() {
        let backend =
            Mock::new(1, [SbiRet { error: 0, value: 2 }, SbiRet { error: 0, value: 2 }, OK]);
        let console = Console::probe_with(&backend);

        console.write_with(&backend, b"molt");

        assert_eq!(backend.debug_calls.get(), [4, 2, 0]);
        assert_eq!(backend.debug_count.get(), 2);
        assert_eq!(backend.legacy().1, 0);
    }

    #[test]
    fn dbcn_error_falls_back_for_remainder() {
        let backend =
            Mock::new(1, [SbiRet { error: 0, value: 1 }, SbiRet { error: -1, value: 0 }, OK]);
        let console = Console::probe_with(&backend);

        console.write_with(&backend, b"molt");

        assert_eq!(backend.debug_calls.get(), [4, 3, 0]);
        assert_eq!(backend.legacy(), ([b'o', b'l', b't', 0, 0, 0, 0, 0], 3));
    }

    #[test]
    fn zero_dbcn_progress_falls_back_to_legacy() {
        let backend = Mock::new(1, [OK; 3]);
        let console = Console::probe_with(&backend);

        console.write_with(&backend, b"molt");

        assert_eq!(backend.debug_calls.get(), [4, 0, 0]);
        assert_eq!(backend.debug_count.get(), 1);
        assert_eq!(backend.legacy(), ([b'm', b'o', b'l', b't', 0, 0, 0, 0], 4));
    }
}
