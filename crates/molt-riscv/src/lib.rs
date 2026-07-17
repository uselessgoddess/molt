#![no_std]

//! RISC-V supervisor binary interface hardware implementations.

#[cfg(target_arch = "riscv64")]
mod riscv64 {
    use core::arch::asm;

    use molt_arch::{ExitStatus, Platform, SerialPort};

    const SBI_CONSOLE_PUTCHAR: usize = 0x01;
    const SBI_SYSTEM_RESET: usize = 0x5352_5354;

    /// Hardware services provided through the RISC-V supervisor binary interface.
    pub struct RiscV {
        serial: SbiSerial,
    }

    impl RiscV {
        pub const fn new() -> Self {
            Self { serial: SbiSerial }
        }
    }

    impl Default for RiscV {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Platform for RiscV {
        type Serial = SbiSerial;

        fn serial(&mut self) -> &mut Self::Serial {
            &mut self.serial
        }

        fn terminate(&mut self, status: ExitStatus) -> ! {
            let reason = match status {
                ExitStatus::Success => 0,
                ExitStatus::Failure => 1,
            };
            // SAFETY: the SBI system-reset extension accepts reset type and reason in a0/a1.
            unsafe {
                sbi_call(SBI_SYSTEM_RESET, 0, 0, reason);
            }
            loop {
                // SAFETY: a failed reset leaves this hart with no remaining kernel work.
                unsafe {
                    asm!("wfi", options(nomem, nostack));
                }
            }
        }
    }

    /// Diagnostic output through the SBI legacy console extension.
    pub struct SbiSerial;

    impl SerialPort for SbiSerial {
        fn write_byte(&mut self, byte: u8) {
            // SAFETY: the legacy console extension accepts the byte value in a0.
            unsafe {
                sbi_call(SBI_CONSOLE_PUTCHAR, 0, byte as usize, 0);
            }
        }
    }

    unsafe fn sbi_call(extension: usize, function: usize, arg0: usize, arg1: usize) -> isize {
        let error: isize;
        // SAFETY: register placement follows the SBI calling convention and ecall preserves memory.
        unsafe {
            asm!(
                "ecall",
                inlateout("a0") arg0 as isize => error,
                in("a1") arg1,
                in("a6") function,
                in("a7") extension,
                options(nostack)
            );
        }
        error
    }
}

#[cfg(target_arch = "riscv64")]
pub use riscv64::{RiscV, SbiSerial};
