#![no_std]

//! RISC-V supervisor boot adaptation and hardware implementations.
//!
//! OpenSBI runs in machine mode, loads the kernel ELF at the fixed S-mode
//! payload address, and jumps to [`_start`]. That shim sets up the boot stack,
//! clears `.bss`, and calls the `__molt_riscv_main` entry the [`entry_point!`]
//! macro generates in the kernel binary. From there [`start`] builds the
//! architecture-neutral [`BootInfo`] and hands control to the shared kernel.
//!
//! Every module that touches supervisor hardware is gated on the RISC-V target
//! so the crate still compiles to an empty shell for host unit tests.

/// Physical base of the QEMU `virt` board's RAM, below the S-mode payload.
#[cfg(target_arch = "riscv64")]
pub(crate) const RAM_BASE: usize = 0x8000_0000;

#[cfg(target_arch = "riscv64")]
mod csr;
#[cfg(target_arch = "riscv64")]
mod paging;
#[cfg(target_arch = "riscv64")]
mod sbi;
#[cfg(target_arch = "riscv64")]
mod trap;

/// Defines the RISC-V entry glue outside `molt-kernel`.
///
/// The [`_start`] shim in this crate calls the `__molt_riscv_main` symbol this
/// macro emits, so linking any Molt kernel against `molt-riscv` wires the boot
/// path automatically and no kernel binary can forget to provide an entry.
#[macro_export]
macro_rules! entry_point {
    ($path:path) => {
        /// Rust boot entry invoked by the assembly `_start` shim.
        ///
        /// # Safety
        ///
        /// Called exactly once by `_start` with the OpenSBI-provided hart id and
        /// device-tree pointer, on the initialized boot stack.
        #[unsafe(no_mangle)]
        extern "C" fn __molt_riscv_main(hartid: usize, device_tree: usize) -> ! {
            $crate::start(hartid, device_tree, $path)
        }
    };
}

#[cfg(target_arch = "riscv64")]
pub use imp::{RiscV, SbiSerial, start};

#[cfg(target_arch = "riscv64")]
mod imp {
    use core::arch::{asm, global_asm};

    use molt_arch::{
        BootInfo, ExitStatus, FRAME_SIZE, MemoryMap, MemoryRegion, MemoryRegionKind, Platform,
        PlatformError, SerialPort,
    };

    use crate::{csr, paging, sbi, trap};

    /// One past the top of the QEMU `virt` board's default 128 MiB of RAM.
    const RAM_END: u64 = 0x8800_0000;

    global_asm!(
        r#"
.section .text._start, "ax"
.global _start
_start:
    csrw    sie, zero          // mask every supervisor interrupt until the vector is set
    csrci   sstatus, 2         // clear sstatus.SIE so traps stay disabled during setup
    la      sp, __molt_stack_top
    la      t0, __bss_start    // zero .bss with doubleword stores (bounds are 8-aligned)
    la      t1, __bss_end
0:
    bgeu    t0, t1, 1f
    sd      zero, 0(t0)
    addi    t0, t0, 8
    j       0b
1:
    call    __molt_riscv_main  // a0 = hartid, a1 = device tree, both from OpenSBI
2:
    wfi                        // a conforming entry never returns; park if it does
    j       2b
"#
    );

    /// Builds the architecture-neutral boot state and starts the shared kernel.
    #[doc(hidden)]
    pub fn start(
        _hartid: usize,
        _device_tree: usize,
        kernel: fn(BootInfo<'_>, &mut RiscV) -> !,
    ) -> ! {
        let memory_map = RiscVMemoryMap::new();
        // Sv39 identity-maps physical memory, so the physical offset is zero.
        let boot_info = BootInfo::new(&memory_map, Some(0));
        let mut platform = RiscV::new();
        kernel(boot_info, &mut platform)
    }

    #[cfg(target_os = "none")]
    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
        molt_arch::panic_handler::<RiscV>(info)
    }

    /// The single usable RAM span left after the loaded kernel image.
    struct RiscVMemoryMap {
        usable_start: u64,
    }

    impl RiscVMemoryMap {
        fn new() -> Self {
            unsafe extern "C" {
                /// End of the loaded image and boot stack, defined by the linker script.
                static __kernel_end: u8;
            }
            // SAFETY: taking the address of a linker-defined symbol is always sound.
            let end = (&raw const __kernel_end) as u64;
            let usable_start = (end + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
            Self { usable_start }
        }
    }

    impl MemoryMap for RiscVMemoryMap {
        fn len(&self) -> usize {
            1
        }

        fn region(&self, index: usize) -> Option<MemoryRegion> {
            match index {
                0 => Some(MemoryRegion::new(self.usable_start, RAM_END, MemoryRegionKind::Usable)),
                _ => None,
            }
        }
    }

    /// Concrete services for the current RISC-V supervisor boot target.
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

        fn initialize(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
            trap::init();
            Ok(())
        }

        fn verify_exception_path(&mut self) -> bool {
            trap::verify_breakpoint()
        }

        fn verify_owned_mapping(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
            paging::verify_owned_mapping(boot_info)
        }

        fn arm_timer(&mut self, initial_count: u32) -> Result<(), PlatformError> {
            // Program an absolute deadline `initial_count` timebase ticks ahead,
            // then unmask the supervisor timer interrupt the trap vector counts.
            let deadline = csr::time().wrapping_add(u64::from(initial_count));
            sbi::set_timer(deadline);
            // SAFETY: `initialize` installed the trap vector before any timer arms.
            unsafe {
                csr::enable_timer_interrupts();
            }
            Ok(())
        }

        fn monotonic_ticks(&self) -> u64 {
            trap::ticks()
        }

        fn wait_for_timer_change(&mut self, previous: u64) {
            while trap::ticks() == previous {
                // SAFETY: with the timer interrupt unmasked, `wfi` resumes on the tick.
                unsafe {
                    asm!("wfi", options(nomem, nostack));
                }
            }
        }

        fn terminate(&mut self, status: ExitStatus) -> ! {
            sbi::shutdown(matches!(status, ExitStatus::Success))
        }
    }

    /// Diagnostic output through the SBI legacy console extension.
    pub struct SbiSerial;

    impl SerialPort for SbiSerial {
        fn write_byte(&mut self, byte: u8) {
            sbi::console_putchar(byte);
        }
    }
}
