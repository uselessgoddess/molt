use molt_arch::{
    BootInfo, FrameAllocator as BootFrameAllocator, MapPermissions, MappingError, PlatformError,
};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

const TEST_PAGE: u64 = 0x0000_5555_5555_0000;

pub fn verify_owned_mapping(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
    let mut frames = X86Frames(BootFrameAllocator::new(boot_info.memory_map()));
    // SAFETY: this probe is the sole page-table owner during single-core boot, and the
    // bootloader-provided direct map covers every physical page-table frame.
    let level_4 = unsafe { active_level_4_table(VirtAddr::new(offset)) };
    // SAFETY: `level_4` is the active table and `offset` is its complete physical direct map.
    let mut mapper = unsafe { OffsetPageTable::new(level_4, VirtAddr::new(offset)) };
    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let page = Page::containing_address(VirtAddr::new(TEST_PAGE));
    let mut mapping = OwnedPage::map(&mut mapper, &mut frames, page, permissions)?;
    mapping.write_and_verify(0x4d4f_4c54_5f57_585e)?;
    drop(mapping);
    Ok(())
}

struct X86Frames<'map>(BootFrameAllocator<'map>);

// SAFETY: `BootFrameAllocator` advances monotonically through firmware regions marked usable,
// so this adapter returns each aligned physical frame at most once.
unsafe impl FrameAllocator<Size4KiB> for X86Frames<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.0.allocate().map(|frame| PhysFrame::containing_address(PhysAddr::new(frame.start())))
    }
}

struct OwnedPage<'m, 't> {
    mapper: &'m mut OffsetPageTable<'t>,
    page: Page<Size4KiB>,
}

impl<'m, 't> OwnedPage<'m, 't> {
    fn map(
        mapper: &'m mut OffsetPageTable<'t>,
        frames: &mut X86Frames<'_>,
        page: Page<Size4KiB>,
        permissions: MapPermissions,
    ) -> Result<Self, PlatformError> {
        let frame =
            frames.allocate_frame().ok_or(PlatformError::Mapping(MappingError::OutOfFrames))?;
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE;
        if permissions.is_writable() {
            flags |= PageTableFlags::WRITABLE;
        }
        if permissions.is_executable() {
            flags.remove(PageTableFlags::NO_EXECUTE);
        }
        // SAFETY: TEST_PAGE is a dedicated, otherwise-unused virtual page and `frame` is a fresh
        // unique frame. W^X was validated by `MapPermissions` before flags were constructed.
        unsafe { mapper.map_to(page, frame, flags, frames) }.map_err(map_error)?.flush();
        Ok(Self { mapper, page })
    }

    fn write_and_verify(&mut self, value: u64) -> Result<(), PlatformError> {
        let pointer = self.page.start_address().as_mut_ptr::<u64>();
        // SAFETY: the owned page is present, writable, uniquely mapped, aligned, and remains
        // alive for both volatile accesses.
        unsafe {
            pointer.write_volatile(value);
            if pointer.read_volatile() != value {
                return Err(PlatformError::Mapping(MappingError::Backend));
            }
        }
        Ok(())
    }
}

impl Drop for OwnedPage<'_, '_> {
    fn drop(&mut self) {
        if let Ok((_frame, flush)) = self.mapper.unmap(self.page) {
            flush.flush();
        }
    }
}

fn map_error(_error: MapToError<Size4KiB>) -> PlatformError {
    PlatformError::Mapping(MappingError::Backend)
}

unsafe fn active_level_4_table(physical_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let physical = level_4_frame.start_address().as_u64();
    let virtual_address = physical_offset + physical;
    let pointer = virtual_address.as_mut_ptr();
    // SAFETY: the caller guarantees a complete direct map and unique access during early boot.
    unsafe { &mut *pointer }
}
