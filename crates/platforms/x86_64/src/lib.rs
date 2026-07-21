#![no_std]
#![feature(abi_x86_interrupt)]

//! x86_64 boot adaptation and hardware implementations.

pub mod acpi;

mod apic;
mod interrupts;
mod memory;
mod msi;

use core::arch::asm;

#[doc(hidden)]
pub use bootloader_api::config::Mapping as BootMapping;
use bootloader_api::info::{MemoryRegionKind as BootMemoryRegionKind, MemoryRegions};
#[doc(hidden)]
pub use bootloader_api::{
    BootInfo as BootloaderInfo, BootloaderConfig, entry_point as __bootloader_entry_point,
};
use molt_arch::memory::{Device, Rights};
use molt_arch::{
    BootInfo, ConfigSpace, DeviceMapper, ExitStatus, FabricError, FramePool, ImageRange,
    InterruptFabric, MappingError, MemoryMap, MemoryRegion, MemoryRegionKind, Mmio, MsiMessage,
    Platform, PlatformError, SerialPort, Sink,
};

/// Defines the bootloader-specific entry wrapper outside `molt-kernel`.
#[macro_export]
macro_rules! entry_point {
    ($path:path) => {
        static __MOLT_BOOT_CONFIG: $crate::BootloaderConfig = {
            let mut config = $crate::BootloaderConfig::new_default();
            config.mappings.physical_memory = Some($crate::BootMapping::Dynamic);
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
    // The RSDP is captured here because this is the only place it exists: it is
    // a field of the bootloader's structure, not of the memory map, and there
    // is no way to find it again later that is not a guess.
    let mut platform = X86_64::new().with_rsdp(raw.rsdp_addr.as_ref().copied());
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

/// Where device register windows are mapped.
///
/// A region of its own, far from the kernel image and from the bootloader's
/// direct map, so that a stray pointer into device space is a fault rather than
/// a write into somebody's data.
pub(crate) const DEVICE_REGION: u64 = 0x0000_4444_0000_0000;

/// How many page-table frames are set aside for device mappings.
///
/// Enough for a few windows' worth of intermediate tables — every window needs
/// at most one table per level below the top — with room to spare. Running out
/// is a bounded, reported failure rather than a corrupted table.
const TABLE_FRAMES: usize = 32;

pub(crate) type TableFrames = FramePool<TABLE_FRAMES>;

/// Concrete services for the current x86_64 boot target.
pub struct X86_64 {
    serial: Com1,
    /// The bootloader's direct-map offset, or zero before `initialize`.
    offset: u64,
    /// The physical address firmware reported for the ACPI RSDP.
    rsdp: Option<u64>,
    /// Frames reserved while the firmware memory map was still borrowable.
    tables: TableFrames,
    /// The next free device address; bumps forward and never back.
    devices: u64,
}

impl X86_64 {
    pub const fn new() -> Self {
        Self {
            serial: Com1,
            offset: 0,
            rsdp: None,
            tables: TableFrames::empty(),
            devices: DEVICE_REGION,
        }
    }

    const fn with_rsdp(mut self, rsdp: Option<u64>) -> Self {
        self.rsdp = rsdp;
        self
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
        let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
        self.offset = offset;

        // The page-table frames are taken now, while the firmware map is still
        // borrowable. A device mapped later cannot go back and ask for one:
        // the map is gone by then, and building a fresh allocator over it would
        // start handing out the frames the live tables are already made of.
        let floor = boot_info.kernel_image().map_or(0, |image| image.end());
        let mut frames = molt_arch::FrameAllocator::above(boot_info.memory_map(), floor);
        self.tables.fill(&mut frames);

        apic::init(offset)
    }

    fn verify_exception_path(&mut self) -> bool {
        interrupts::verify_breakpoint()
    }

    fn verify_owned_mapping(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
        memory::verify_owned_mapping(offset, &mut self.tables)
    }

    fn verify_image_protection(&mut self, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
        memory::verify_image_protection(offset, boot_info)
    }

    /// The configuration space ACPI's MCFG table describes.
    ///
    /// Firmware reports the RSDP address and molt walks from there rather than
    /// scanning low memory for the signature: the scan is a guess that finds a
    /// stale table on machines that kept one, and UEFI does not put the RSDP
    /// where the scan looks in the first place.
    fn config_space(&mut self, _boot_info: &BootInfo<'_>) -> Result<ConfigSpace, PlatformError> {
        let rsdp = self.rsdp.ok_or(PlatformError::MissingConfigSpace)?;
        if self.offset == 0 {
            return Err(PlatformError::MissingPhysicalMemoryMap);
        }
        // SAFETY: `rsdp` is the address the bootloader took from firmware, and
        // `self.offset` is the direct map it built, valid for the whole run.
        unsafe { acpi::config_space(rsdp, self.offset) }
            .map_err(|_| PlatformError::MissingConfigSpace)
    }

    fn route_interrupts(&mut self, sink: &'static dyn Sink) -> Result<(), PlatformError> {
        msi::route(sink);
        Ok(())
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

impl DeviceMapper for X86_64 {
    fn map_device(
        &mut self,
        window: Device,
        rights: Rights,
    ) -> Result<Mmio<'static>, MappingError> {
        memory::map_device(self.offset, &mut self.tables, &mut self.devices, window, rights)
    }
}

impl InterruptFabric for X86_64 {
    fn allocate(&mut self) -> Result<(u16, MsiMessage), FabricError> {
        msi::allocate()
    }

    fn release(&mut self, line: u16) -> Result<(), FabricError> {
        msi::release(line)
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
