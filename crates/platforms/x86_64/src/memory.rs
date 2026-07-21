use molt_arch::audit::{Audit, Leaf, MappedRange, PageWalk};
use molt_arch::memory::{Cache, Device, Rights};
use molt_arch::{BootInfo, MapPermissions, MappingError, Mmio, PageProtection, PlatformError};
use x86_64::registers::control::Cr3;
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::mapper::{MapToError, TranslateResult};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::{DEVICE_REGION, TableFrames};

const TEST_PAGE: u64 = 0x0000_5555_5555_0000;

/// How far the device region reaches.
///
/// A gigabyte of address space for register windows, in a part of the address
/// space nothing else uses. It is a bound rather than a policy: exceeding it
/// means a driver asked for far more than registers, and refusing is better
/// than walking off into whatever the bootloader mapped next.
const DEVICE_REGION_END: u64 = DEVICE_REGION + (1 << 30);

/// The page-attribute-table MSR, and the entry `PCD | PWT` selects.
///
/// The reset value of entry 3 is uncacheable, and nothing in molt reprograms
/// the PAT. [`verify_uncached`] checks rather than assumes, because a device
/// window that is quietly write-back is a bug that shows up as a device that
/// works until it doesn't.
const IA32_PAT: u32 = 0x277;
const PAT_ENTRY_UC: u8 = 0x00;
const PAT_DEVICE_ENTRY: u32 = 3;

pub fn verify_owned_mapping(offset: u64, pool: &mut TableFrames) -> Result<(), PlatformError> {
    let mut frames = X86Frames(pool);
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

pub fn verify_image_protection(offset: u64, boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let image = boot_info.kernel_image().ok_or(PlatformError::Mapping(MappingError::Unmapped))?;
    // SAFETY: single-core boot, and the bootloader's direct map covers every
    // physical page-table frame.
    let level_4 = unsafe { active_level_4_table(VirtAddr::new(offset)) };
    // SAFETY: `level_4` is the active table and `offset` is its complete direct map.
    let mapper = unsafe { OffsetPageTable::new(level_4, VirtAddr::new(offset)) };

    let ranges = [MappedRange::image(image.start(), image.end())];
    let walk = MapperWalk { mapper: &mapper };
    Audit::new(&ranges).cover(&walk).map_err(PlatformError::Mapping)
}

/// Maps `window` at the next free device address and hands back its registers.
///
/// `cursor` is a bump pointer through the device region and never moves back,
/// so a window is never re-issued at an address a released one used to hold.
/// Unmapping is deliberately absent: nothing in Stage 2.2 releases a device,
/// and an unmap that races a driver still holding an [`Mmio`] is exactly the
/// bug the borrow on the window exists to prevent.
///
/// # What this does not fix
///
/// The bootloader's direct map already covers this physical range write-back,
/// so the registers have a cacheable alias at `offset + physical` for as long
/// as the kernel runs on the bootloader's tables. Nothing here reads through
/// that alias, but nothing prevents it either; removing it needs the kernel to
/// own its own page tables, which is the outstanding Stage 2.1 item.
pub fn map_device(
    offset: u64,
    pool: &mut TableFrames,
    cursor: &mut u64,
    window: Device,
    rights: Rights,
) -> Result<Mmio<'static>, MappingError> {
    if offset == 0 {
        // `initialize` records the direct-map offset; a zero here means a
        // driver asked for a window before the platform was brought up.
        return Err(MappingError::Unmapped);
    }
    // The window decides its own cacheability; the caller only says whether it
    // needs to write. An executable or write-back device mapping is refused
    // here, before a single page table is touched.
    let (rights, cache) = window.mapping(rights)?;
    verify_uncached(cache)?;

    let span = window.span();
    let bytes = span.bytes();
    let base = *cursor;
    let end = base.checked_add(bytes).ok_or(MappingError::InvalidAddress)?;
    if end > DEVICE_REGION_END {
        return Err(MappingError::OutOfFrames);
    }

    let mut flags = PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE;
    if rights.is_write() {
        flags |= PageTableFlags::WRITABLE;
    }
    if cache == Cache::Device {
        // `PCD | PWT` selects PAT entry 3, which `verify_uncached` just
        // confirmed is uncacheable.
        flags |= PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH;
    }

    let mut frames = X86Frames(pool);
    // SAFETY: single-core boot, and the bootloader's direct map covers every
    // physical page-table frame.
    let level_4 = unsafe { active_level_4_table(VirtAddr::new(offset)) };
    // SAFETY: `level_4` is the active table and `offset` is its complete direct map.
    let mut mapper = unsafe { OffsetPageTable::new(level_4, VirtAddr::new(offset)) };

    let mut address = 0;
    while address < bytes {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(base + address));
        // A translation that already exists means the bump pointer and the
        // live tables disagree about what is free. Overwriting it would unmap
        // something silently, so this fails closed instead.
        if !matches!(mapper.translate(page.start_address()), TranslateResult::NotMapped) {
            return Err(MappingError::Unexpected);
        }
        let frame =
            PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(span.start() + address));
        // SAFETY: `Inventory::device` established that this physical range is
        // not RAM and not the kernel image, the virtual page was just proven
        // unmapped, and the flags carry no execute permission.
        unsafe { mapper.map_to(page, frame, flags, &mut frames) }
            .map_err(|_| MappingError::Backend)?
            .flush();
        address += molt_arch::FRAME_SIZE;
    }

    // Round the cursor out to a 2 MiB boundary so two windows never share a
    // page-table leaf's neighbourhood, and so an off-by-one in a driver's
    // offset arithmetic lands in a hole rather than in another device.
    *cursor = end.next_multiple_of(2 * 1024 * 1024);
    // SAFETY: every frame of `span` was just mapped at `base`, uncached and
    // non-executable, and the mapping is never removed, so the window stays
    // valid for `'static`. The bump cursor guarantees no second window over
    // the same virtual range.
    Ok(unsafe { Mmio::new(base as *mut u8, bytes) })
}

/// Confirms the MMU will actually make a `PCD | PWT` mapping uncached.
///
/// Reading the MSR rather than trusting the reset value is the difference
/// between "the flags say device" and "the memory behaves like a device". A PAT
/// somebody reprogrammed fails the mapping instead of returning a window whose
/// writes sit in a cache line.
fn verify_uncached(cache: Cache) -> Result<(), MappingError> {
    if cache != Cache::Device {
        return Ok(());
    }
    // SAFETY: IA32_PAT is architectural on every CPU molt boots on — the same
    // CPUID-checked feature set the local APIC needs — and this only reads it.
    let pat = unsafe { Msr::new(IA32_PAT).read() };
    let entry = (pat >> (PAT_DEVICE_ENTRY * 8)) as u8;
    if entry == PAT_ENTRY_UC { Ok(()) } else { Err(MappingError::Cacheability) }
}

/// The memory type a leaf's cacheability bits select.
///
/// Only the exact encoding [`map_device`] writes counts as [`Cache::Device`].
/// Any other combination is reported write-back, which is the answer that makes
/// an audit of a device range fail — a leaf whose memory type this code does not
/// recognise is precisely the leaf nobody should trust to be uncached.
fn cache_of(flags: PageTableFlags) -> Cache {
    let device =
        flags.contains(PageTableFlags::NO_CACHE) && flags.contains(PageTableFlags::WRITE_THROUGH);
    if device { Cache::Device } else { Cache::WriteBack }
}

/// [`PageWalk`] over the live x86_64 tables the bootloader built.
struct MapperWalk<'m, 't> {
    mapper: &'m OffsetPageTable<'t>,
}

impl PageWalk for MapperWalk<'_, '_> {
    fn leaf(&self, address: u64) -> Option<Leaf> {
        // Report the actual leaf size the loader used, not always 4 KiB, so a
        // 2 MiB huge page across the image surfaces as a granularity error.
        let TranslateResult::Mapped { frame, flags, .. } =
            self.mapper.translate(VirtAddr::new(address))
        else {
            return None;
        };
        let size = frame.size();
        let start = address & !(size - 1);
        let protection = PageProtection::new(
            flags.contains(PageTableFlags::PRESENT),
            flags.contains(PageTableFlags::WRITABLE),
            !flags.contains(PageTableFlags::NO_EXECUTE),
        )
        .cached(cache_of(flags));
        Some(Leaf::new(start, size, protection))
    }
}

struct X86Frames<'pool>(&'pool mut TableFrames);

// SAFETY: a `FramePool` is filled once from a `FrameAllocator` that walks the firmware map
// monotonically and hands out each frame at most once, so this adapter does too.
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
        if permissions.is_write() {
            flags |= PageTableFlags::WRITABLE;
        }
        if permissions.is_execute() {
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
