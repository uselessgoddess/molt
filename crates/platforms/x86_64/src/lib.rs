#![no_std]
#![feature(abi_x86_interrupt)]

//! x86_64 boot adaptation and hardware implementations.

mod acpi;
mod apic;
mod interrupts;
mod memory;
mod pci;

use core::arch::asm;

#[doc(hidden)]
pub use bootloader_api::config::Mapping as BootMapping;
use bootloader_api::info::{MemoryRegionKind as BootMemoryRegionKind, MemoryRegions};
#[doc(hidden)]
pub use bootloader_api::{
    BootInfo as BootloaderInfo, BootloaderConfig, entry_point as __bootloader_entry_point,
};
use molt_arch::{
    BootInfo, DeviceFunction, ExitStatus, ImageRange, InterruptSink, MemoryMap, MemoryRegion,
    MemoryRegionKind, Platform, PlatformError, SerialPort,
};

/// Where the loader must place the boot stack, and how large it is.
///
/// The kernel re-creates the stack mapping in its own tables before switching
/// `CR3`, and it cannot ask the loader afterwards where the stack went — the
/// boot info does not say. Pinning the address is what makes the window
/// findable, so it is a fixed address rather than a dynamic one.
pub const STACK_BASE: u64 = 0xffff_9000_0000_0000;
pub const STACK_SIZE: u64 = 128 * 1024;

/// Where the loader must place [`BootloaderInfo`], and the window the kernel
/// clones around it. The structure's length depends on the firmware memory
/// map, so the window is sized for the largest map and holes are skipped.
pub const BOOT_INFO_BASE: u64 = 0xffff_9100_0000_0000;
pub const BOOT_INFO_WINDOW: u64 = 2 * 1024 * 1024;

/// Defines the bootloader-specific entry wrapper outside `molt-kernel`.
#[macro_export]
macro_rules! entry_point {
    ($path:path) => {
        static __MOLT_BOOT_CONFIG: $crate::BootloaderConfig = {
            let mut config = $crate::BootloaderConfig::new_default();
            config.mappings.physical_memory = Some($crate::BootMapping::Dynamic);
            config.mappings.kernel_stack = $crate::BootMapping::FixedAddress($crate::STACK_BASE);
            config.kernel_stack_size = $crate::STACK_SIZE;
            config.mappings.boot_info = $crate::BootMapping::FixedAddress($crate::BOOT_INFO_BASE);
            config
        };

        fn __molt_x86_64_entry(boot_info: &'static mut $crate::BootloaderInfo) -> ! {
            $crate::start(boot_info, $path)
        }

        $crate::__bootloader_entry_point!(__molt_x86_64_entry, config = &__MOLT_BOOT_CONFIG);
    };
}

#[doc(hidden)]
pub fn start(raw: &'static mut BootloaderInfo, kernel: fn(BootInfo<'_>, &mut X86_64) -> !) -> ! {
    let memory_map = BootloaderMemoryMap::new(&raw.memory_regions);
    let physical_memory_offset = raw.physical_memory_offset.as_ref().copied();
    let kernel_image = ImageRange::new(raw.kernel_image_offset, raw.kernel_len);
    let boot_info =
        BootInfo::new(&memory_map, physical_memory_offset).with_kernel_image(kernel_image);
    // The firmware description tables are not in the memory map and not in the
    // boot info the kernel sees; the loader reports where they start and
    // nothing else knows. Keeping it here rather than widening `BootInfo` is
    // deliberate: ACPI is this platform's business, not the kernel's.
    acpi::remember(raw.rsdp_addr.into_option());
    let mut platform = X86_64::new();
    kernel(boot_info, &mut platform)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    molt_arch::panic_handler::<X86_64>(info)
}

struct BootloaderMemoryMap<'map> {
    regions: &'map MemoryRegions,
}

impl<'map> BootloaderMemoryMap<'map> {
    const fn new(regions: &'map MemoryRegions) -> Self {
        Self { regions }
    }
}

impl MemoryMap for BootloaderMemoryMap<'_> {
    fn len(&self) -> usize {
        self.regions.len()
    }

    fn region(&self, index: usize) -> Option<MemoryRegion> {
        self.regions.get(index).map(|region| {
            let kind = match region.kind {
                BootMemoryRegionKind::Usable => MemoryRegionKind::Usable,
                BootMemoryRegionKind::Bootloader => MemoryRegionKind::Bootloader,
                BootMemoryRegionKind::UnknownUefi(tag) | BootMemoryRegionKind::UnknownBios(tag) => {
                    MemoryRegionKind::Firmware(tag)
                }
                _ => MemoryRegionKind::Reserved,
            };
            MemoryRegion::new(region.start, region.end, kind)
        })
    }
}

/// Concrete services for the current x86_64 boot target.
pub struct X86_64 {
    serial: Com1,
}

impl X86_64 {
    pub const fn new() -> Self {
        Self { serial: Com1 }
    }
}

impl Default for X86_64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for X86_64 {
    type Serial = Com1;

    fn serial(&mut self) -> &mut Self::Serial {
        &mut self.serial
    }

    fn initialize(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        interrupts::init();
        // The kernel's own tables come up before the APIC, not after: the
        // loader's direct map is the only way to reach MMIO until they exist,
        // and it stops being live the moment `CR3` is written.
        let windows = memory::init(boot_info)?;
        apic::init(windows.apic)?;
        pci::init(windows.configuration);
        Ok(())
    }

    fn verify_exception_path(&mut self) -> bool {
        interrupts::verify_breakpoint()
    }

    fn verify_owned_mapping(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        memory::verify_owned_mapping(boot_info)
    }

    fn verify_image_protection(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        memory::verify_image_protection(boot_info)
    }

    fn verify_device_window(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        memory::verify_device_window(boot_info)
    }

    fn attach(&mut self, sink: &'static dyn InterruptSink) {
        interrupts::attach(sink);
    }

    fn enumerate(&mut self, found: &mut dyn FnMut(DeviceFunction)) -> Result<(), PlatformError> {
        pci::enumerate(found)
    }

    fn raise_message_interrupt(&mut self, boot_info: &BootInfo<'_>) -> Result<u8, PlatformError> {
        pci::raise_message_interrupt(boot_info)
    }

    fn verify_message_table(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        pci::verify_message_table(boot_info)
    }

    fn arm_timer(&mut self, initial_count: u32) -> Result<(), PlatformError> {
        apic::arm(initial_count)
    }

    fn monotonic_ticks(&self) -> u64 {
        apic::ticks()
    }

    fn wait_for_timer_change(&mut self, previous: u64) {
        apic::wait_for_change(previous);
    }

    fn terminate(&mut self, status: ExitStatus) -> ! {
        let code = match status {
            ExitStatus::Success => 0x10,
            ExitStatus::Failure => 0x11,
        };
        // SAFETY: 0xf4 is reserved for the isa-debug-exit device by the image runner.
        unsafe {
            out_u32(0xf4, code);
        }
        halt_forever()
    }
}

/// The legacy 16550 UART at the standard COM1 I/O base.
pub struct Com1;

impl SerialPort for Com1 {
    fn init(&mut self) {
        // SAFETY: the boot target reserves these standard COM1 registers for this driver.
        unsafe {
            out_u8(0x3f9, 0x00);
            out_u8(0x3fb, 0x80);
            out_u8(0x3f8, 0x03);
            out_u8(0x3f9, 0x00);
            out_u8(0x3fb, 0x03);
            out_u8(0x3fa, 0xc7);
            out_u8(0x3fc, 0x0b);
        }
    }

    fn write_byte(&mut self, byte: u8) {
        // SAFETY: COM1 was initialized above and this driver exclusively owns its registers.
        unsafe {
            while in_u8(0x3fd) & 0x20 == 0 {
                core::hint::spin_loop();
            }
            out_u8(0x3f8, byte);
        }
    }
}

pub(crate) fn halt_forever() -> ! {
    loop {
        // SAFETY: the kernel has no work after reporting its terminal state.
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}

pub(crate) fn emergency_write(text: &str) {
    for byte in text.bytes() {
        emergency_byte(byte);
    }
}

pub(crate) fn emergency_byte(byte: u8) {
    // SAFETY: fatal exception diagnostics use the already-initialized COM1 UART exclusively.
    unsafe {
        while in_u8(0x3fd) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        out_u8(0x3f8, byte);
    }
}

unsafe fn out_u8(port: u16, value: u8) {
    // SAFETY: callers own the I/O port they pass to this architecture-private function.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

unsafe fn out_u32(port: u16, value: u32) {
    // SAFETY: callers own the I/O port they pass to this architecture-private function.
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

unsafe fn in_u8(port: u16) -> u8 {
    let value: u8;
    // SAFETY: callers own the I/O port they pass to this architecture-private function.
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::boxed::Box;

    use bootloader_api::info::{
        MemoryRegion as BootMemoryRegion, MemoryRegionKind as BootMemoryRegionKind, MemoryRegions,
    };
    use molt_arch::{MemoryMap, MemoryRegion, MemoryRegionKind};

    use super::BootloaderMemoryMap;

    #[test]
    fn bootloader_memory() {
        let raw = Box::leak(Box::new([
            BootMemoryRegion { start: 0, end: 4096, kind: BootMemoryRegionKind::Bootloader },
            BootMemoryRegion { start: 4096, end: 8192, kind: BootMemoryRegionKind::Usable },
            BootMemoryRegion {
                start: 8192,
                end: 12288,
                kind: BootMemoryRegionKind::UnknownUefi(7),
            },
        ]));
        let regions = MemoryRegions::from(&mut raw[..]);
        let map = BootloaderMemoryMap::new(&regions);

        assert_eq!(map.len(), 3);
        assert_eq!(map.region(0), Some(MemoryRegion::new(0, 4096, MemoryRegionKind::Bootloader)));
        assert_eq!(map.region(1), Some(MemoryRegion::new(4096, 8192, MemoryRegionKind::Usable)));
        assert_eq!(
            map.region(2),
            Some(MemoryRegion::new(8192, 12288, MemoryRegionKind::Firmware(7)))
        );
        assert_eq!(map.region(3), None);
    }
}
