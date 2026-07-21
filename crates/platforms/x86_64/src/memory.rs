//! The x86_64 address space the kernel builds for itself.
//!
//! Until now the bootloader's tables stayed live for the whole of boot. That
//! made [`Audit::accepts`] impossible on this platform: the loader maps its own
//! framebuffer, page tables, and identity regions, so a sweep of the live
//! tables would have reported dozens of leaves the kernel never declared, and
//! the check had to be dropped to the outward-only [`Audit::cover`]. It also
//! left nowhere to put an MMIO window with its own memory type, because
//! everything reachable went through the loader's write-back direct map — the
//! local APIC included.
//!
//! [`init`] therefore builds a fresh four-level table from frames the kernel
//! allocates, containing exactly four things: a direct map of firmware-usable
//! RAM, the kernel image cloned page by page with the rights the loader gave
//! each page, the pinned boot stack and boot-info windows, and the APIC device
//! window. Then it loads `CR3`. Everything the CPU touches after that
//! instruction — the code it is executing, its stack, the tables themselves —
//! is in that list, and everything in that list is declared, so both directions
//! of the audit run here.

use core::cell::UnsafeCell;

use molt_arch::audit::{Audit, Contents, Declared, Leaf, MappedRange, PageWalk};
use molt_arch::memory::{Cache, Device, Inventory, Rights, Span};
use molt_arch::{
    BootInfo, FrameAllocator as BootFrameAllocator, FrameCursor, MapPermissions, MappingError,
    PageProtection, PlatformError, UsableRegions,
};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::mapper::{MapToError, TranslateResult};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size2MiB, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::{BOOT_INFO_BASE, BOOT_INFO_WINDOW, STACK_BASE, STACK_SIZE};

/// How far past the reported image length `mapped_end` will follow the loader.
///
/// This is a runaway guard, not a size the kernel is expected to reach: the
/// walk stops at the loader's first unmapped page, and the kernel image plus
/// its `.bss` is a couple of megabytes. Without a bound, a loader that mapped
/// the image right up against another window would make the walk cross into
/// it, one 4 KiB page walk at a time. 64 MiB caps that at 16K probes — a few
/// milliseconds even under QEMU — while leaving two orders of magnitude of
/// headroom over the real image.
const IMAGE_LIMIT: u64 = 64 * 1024 * 1024;

const TEST_PAGE: u64 = 0x0000_5555_5555_0000;

/// Where the kernel maps the local APIC.
const APIC_WINDOW: u64 = 0xffff_9200_0000_0000;

/// Where the kernel maps configuration space. The aperture is up to 256 MiB —
/// one 1 MiB window per bus — so it gets a slot of its own rather than a page.
const ECAM_WINDOW: u64 = 0xffff_9300_0000_0000;

/// Where the kernel maps device windows it opens after boot — a BAR a driver
/// asked for. Windows are handed out from here upwards, each 2 MiB apart so a
/// register block never shares a leaf with the next one.
const DRIVER_WINDOW: u64 = 0xffff_9400_0000_0000;

/// The stride between successive driver windows.
const WINDOW_STRIDE: u64 = 2 * 1024 * 1024;

/// Declared ranges: the image, two boot windows, the APIC, configuration
/// space, the driver windows, and one entry per usable RAM region — firmware
/// maps are chatty, so this is generous.
type Log = Declared<64>;

/// The device windows [`init`] mapped, in the addresses the kernel now uses.
///
/// Both stop being reachable through the loader's direct map the moment `CR3`
/// changes, and neither has any other name afterwards, so `init` hands them
/// back rather than leaving each driver to find its own.
pub struct Windows {
    pub apic: u64,
    pub configuration: Option<Configuration>,
}

/// Configuration space, mapped, and the buses it answers for.
#[derive(Clone, Copy, Debug)]
pub struct Configuration {
    pub base: u64,
    pub first: u8,
    pub last: u8,
}

/// The address space [`init`] built, kept so later probes can extend it.
struct Space {
    root: PhysFrame<Size4KiB>,
    offset: u64,
    /// Where frame allocation stopped, so a probe does not reissue a table frame.
    cursor: FrameCursor,
    /// Where the next driver window goes.
    window: u64,
    log: Log,
}

struct Active(UnsafeCell<Option<Space>>);

// SAFETY: the address space is built and used on the boot CPU before any other
// core is started, so there is no concurrent access to share.
unsafe impl Sync for Active {}

static ACTIVE: Active = Active(UnsafeCell::new(None));

fn active() -> Result<&'static mut Space, PlatformError> {
    // SAFETY: single boot CPU, interrupt handlers do not touch this cell, and
    // the returned borrow is confined to one call.
    unsafe { &mut *ACTIVE.0.get() }.as_mut().ok_or(PlatformError::Mapping(MappingError::Unmapped))
}

/// Builds the kernel's own address space and switches `CR3` to it.
pub fn init(boot_info: &BootInfo<'_>) -> Result<Windows, PlatformError> {
    let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
    let image = boot_info.kernel_image().ok_or(PlatformError::Mapping(MappingError::Unmapped))?;
    let map = boot_info.memory_map();
    let mut frames = X86Frames(BootFrameAllocator::new(map));
    // Firmware description tables live in reserved memory, which the kernel's
    // own map deliberately does not cover: this is the last moment they can be
    // read at all, so where configuration space is gets settled here.
    //
    // SAFETY: `CR3` still names the loader's tables, whose direct map at
    // `offset` covers every physical address, and it is not written below until
    // this borrow is gone.
    let aperture = crate::acpi::configuration(&unsafe { crate::acpi::Direct::new(offset) })
        // A second segment group would need a second window; nothing the kernel
        // enumerates yet lives outside the first, and mapping one it cannot
        // reach would only declare a range no driver asks for.
        .filter(|found| found.group == 0)
        .and_then(|found| {
            let span = Span::new(found.base, found.base + found.span()).ok()?;
            let window = Inventory::new(map).aperture(span).ok()?;
            Some((found, window))
        });

    let root = frames.allocate_frame().ok_or(out_of_frames())?;
    // SAFETY: the loader's direct map is still live and covers every physical
    // frame, so the fresh root is writable at `offset + root`.
    let table = unsafe { &mut *table_pointer(offset, root) };
    table.zero();
    // SAFETY: `table` is a 512-entry level-4 table and `offset` is the live
    // direct map of all physical memory, which is what the mapper walks with.
    let mut space = unsafe { OffsetPageTable::new(table, VirtAddr::new(offset)) };

    // SAFETY: single-core boot, and the loader's direct map covers every table frame.
    let live = unsafe { active_level_4_table(VirtAddr::new(offset)) };
    // SAFETY: `live` is the table `CR3` names and `offset` is its direct map.
    let live = unsafe { OffsetPageTable::new(live, VirtAddr::new(offset)) };

    let mut log = Log::new();
    direct_map(&mut space, &mut frames, &mut log, map, offset)?;
    // The image, the stack, and the boot info keep the virtual addresses the
    // loader chose: `CR3` is written from code running at its image address,
    // with locals on the boot stack, so any shift would fault on the next
    // instruction. Their rights are read back out of the loader's tables
    // rather than assumed, so a section it mapped wrongly fails the audit
    // instead of being silently re-created correctly here.
    //
    // `kernel_len` is the ELF file length, so it stops short of the `.bss`
    // pages the loader also mapped. Following the loader's mapping to its first
    // hole is what keeps kernel statics — the audit log, the APIC window base —
    // addressable after the switch.
    let image_end = mapped_end(&live, image.start(), image.end())?;
    clone(&mut space, &mut frames, &mut log, &live, image.start(), image_end, Contents::Image)?;
    // The loader places a guard page at the fixed address and the stack above
    // it, so the usable stack ends one page past `STACK_BASE + STACK_SIZE`.
    // Cloning the extra page is what keeps `RSP` mapped across the `CR3` write;
    // unmapped pages inside the window are skipped, so the guard stays a hole.
    let stack_end = STACK_BASE + STACK_SIZE + 2 * Size4KiB::SIZE;
    clone(&mut space, &mut frames, &mut log, &live, STACK_BASE, stack_end, Contents::Ram)?;
    let boot_info_end = BOOT_INFO_BASE + BOOT_INFO_WINDOW;
    clone(&mut space, &mut frames, &mut log, &live, BOOT_INFO_BASE, boot_info_end, Contents::Ram)?;
    device_window(&mut space, &mut frames, &mut log, map, crate::apic::APIC_MMIO, APIC_WINDOW)?;
    let configuration = match aperture {
        Some((segment, window)) => {
            map_device(&mut space, &mut frames, &mut log, window, ECAM_WINDOW)?;
            Some(Configuration { base: ECAM_WINDOW, first: segment.first, last: segment.last })
        }
        None => None,
    };

    // SAFETY: PAT is architectural on every CPU that supports it, and this
    // configuration is the reset one, restated so a firmware that reprogrammed
    // it cannot turn the uncacheable slot the device windows select into
    // something else.
    unsafe { write_pat() };

    let cursor = frames.0.cursor();
    // SAFETY: the executing code, its stack, the boot info, the tables reached
    // through `offset`, and the APIC window are all present in `root`, so
    // translation can be switched over in place. `Cr3Flags::empty()` keeps
    // write-through and cache-disable off for the table walk itself.
    unsafe { Cr3::write(root, Cr3Flags::empty()) };
    // SAFETY: same reasoning as `active`; this runs once on the boot CPU.
    unsafe { *ACTIVE.0.get() = Some(Space { root, offset, cursor, window: DRIVER_WINDOW, log }) };
    Ok(Windows { apic: APIC_WINDOW, configuration })
}

/// Maps every firmware-usable RAM region at `offset + physical`.
///
/// Only usable regions are mapped: firmware, loader, and MMIO holes get no
/// mapping at all, which is what makes a stray leaf over them detectable.
fn direct_map(
    space: &mut OffsetPageTable<'_>,
    frames: &mut X86Frames<'_>,
    log: &mut Log,
    map: &dyn molt_arch::MemoryMap,
    offset: u64,
) -> Result<(), PlatformError> {
    for region in UsableRegions::above(map, 0) {
        // Staying inside the region rather than rounding outward keeps the
        // leaves from covering a neighbour nobody declared.
        let start = align_up(region.start())?;
        let end = align_down(region.end());
        if start >= end {
            continue;
        }
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        let mut physical = start;
        while physical < end {
            let virtual_address = offset.checked_add(physical).ok_or(address_error())?;
            let large = virtual_address % Size2MiB::SIZE == 0
                && physical % Size2MiB::SIZE == 0
                && end - physical >= Size2MiB::SIZE;
            if large {
                let page = Page::<Size2MiB>::containing_address(VirtAddr::new(virtual_address));
                let frame = PhysFrame::containing_address(PhysAddr::new(physical));
                // SAFETY: the target table is not live yet, the frame is plain
                // RAM the firmware reported usable, and the mapping is created
                // once — the loop never revisits a physical address.
                unsafe { space.map_to(page, frame, flags, frames) }.map_err(map_error)?.ignore();
                physical += Size2MiB::SIZE;
            } else {
                map_4k(space, frames, virtual_address, physical, flags)?;
                physical += Size4KiB::SIZE;
            }
        }
        log.push(MappedRange::ram(offset + start, offset + end)).map_err(PlatformError::Mapping)?;
    }
    Ok(())
}

/// Re-creates the loader's mapping of `[start, end)` in the kernel's tables.
///
/// Pages the loader left unmapped — the stack guard page, the tail of the
/// boot-info window — are skipped rather than invented, and each run of cloned
/// pages is declared as it is found, so a hole never turns into a declared
/// range with nothing behind it.
fn clone(
    space: &mut OffsetPageTable<'_>,
    frames: &mut X86Frames<'_>,
    log: &mut Log,
    live: &OffsetPageTable<'_>,
    start: u64,
    end: u64,
    contents: Contents,
) -> Result<(), PlatformError> {
    let mut virtual_address = align_down(start);
    while virtual_address < end {
        let TranslateResult::Mapped { frame, flags, .. } =
            live.translate(VirtAddr::new(virtual_address))
        else {
            virtual_address += Size4KiB::SIZE;
            continue;
        };
        // Round-trip the rights through `Rights` so a page the loader mapped
        // both writable and executable is refused here, not cloned.
        let granted = protection(flags, frame.size());
        let rights = Rights::page_protected(granted).map_err(PlatformError::Mapping)?;
        let physical = frame.start_address().as_u64() + (virtual_address & (frame.size() - 1));
        map_4k(space, frames, virtual_address, physical, leaf_flags(rights, Cache::WriteBack))?;
        log.push(MappedRange::new(virtual_address, virtual_address + Size4KiB::SIZE, contents))
            .map_err(PlatformError::Mapping)?;
        virtual_address += Size4KiB::SIZE;
    }
    Ok(())
}

/// Maps one MMIO frame obtained from [`Inventory::device`], never a raw address.
fn device_window(
    space: &mut OffsetPageTable<'_>,
    frames: &mut X86Frames<'_>,
    log: &mut Log,
    map: &dyn molt_arch::MemoryMap,
    physical: u64,
    virtual_address: u64,
) -> Result<(), PlatformError> {
    let span = Span::frames(physical, 1).map_err(|_| address_error())?;
    // Refuses anything firmware claimed as RAM, so a mistyped register base
    // cannot become a device mapping over the kernel's own memory.
    let window = Inventory::new(map).device(span).map_err(|_| address_error())?;
    map_device(space, frames, log, window, virtual_address)
}

/// Maps a whole device window, however many frames it covers.
///
/// A single register block is one frame, but configuration space is a quarter
/// of a gigabyte, and a leaf per page of it would be a hundred thousand table
/// entries to build and to walk on every audit. Wherever both ends line up, a
/// 2 MiB leaf carries the same rights and the same memory type as the pages it
/// replaces, so the window is described in a hundred-odd leaves instead.
fn map_device(
    space: &mut OffsetPageTable<'_>,
    frames: &mut X86Frames<'_>,
    log: &mut Log,
    window: Device,
    virtual_address: u64,
) -> Result<(), PlatformError> {
    let (rights, cache) = window.mapping(Rights::READ_WRITE).map_err(PlatformError::Mapping)?;
    let flags = leaf_flags(rights, cache);
    let span = window.span();
    let mut physical = span.start();
    while physical < span.end() {
        let at = virtual_address + (physical - span.start());
        let large = at % Size2MiB::SIZE == 0
            && physical % Size2MiB::SIZE == 0
            && span.end() - physical >= Size2MiB::SIZE;
        if large {
            let page = Page::<Size2MiB>::containing_address(VirtAddr::new(at));
            let frame = PhysFrame::containing_address(PhysAddr::new(physical));
            // SAFETY: the frame is inside a window `Inventory` proved firmware
            // did not claim as RAM, and each physical address is mapped once.
            unsafe { space.map_to(page, frame, flags, frames) }.map_err(map_error)?.ignore();
            physical += Size2MiB::SIZE;
        } else {
            map_4k(space, frames, at, physical, flags)?;
            physical += Size4KiB::SIZE;
        }
    }
    log.push(MappedRange::device(virtual_address, virtual_address + span.bytes()))
        .map_err(PlatformError::Mapping)
}

/// Maps `bytes` of MMIO at `physical` where a driver can reach it, and returns
/// the address `physical` itself now has.
///
/// Everything [`init`] maps goes into a table nothing is walking yet. This does
/// not: the leaves appear underneath the running CPU, so each is flushed as it
/// is created rather than at the end, and the window is declared before the
/// next audit runs — a leaf the log has not heard of is exactly what
/// [`Audit::accepts`] exists to catch.
///
/// The span still goes through [`Inventory`], so a BAR firmware programmed over
/// RAM is refused here rather than mapped uncacheable over the kernel's own
/// memory. `physical` need not be page aligned: an MSI-X table is at an offset
/// inside a BAR, and the offset comes back in the returned address.
pub fn open_window(
    boot_info: &BootInfo<'_>,
    physical: u64,
    bytes: u64,
) -> Result<u64, PlatformError> {
    let state = active()?;
    let start = align_down(physical);
    let end = align_up(physical.checked_add(bytes).ok_or(address_error())?)?;
    // One stride is the whole of a window's room; a larger one would overlap
    // its successor, and no register block this kernel opens is that big.
    if end - start > WINDOW_STRIDE {
        return Err(address_error());
    }
    let span = Span::new(start, end).map_err(|_| address_error())?;
    let window =
        Inventory::new(boot_info.memory_map()).device(span).map_err(|_| address_error())?;
    let (rights, cache) = window.mapping(Rights::READ_WRITE).map_err(PlatformError::Mapping)?;
    let flags = leaf_flags(rights, cache);

    let base = state.window;
    let mut frames = X86Frames(BootFrameAllocator::resume(boot_info.memory_map(), state.cursor));
    let mut space = state.mapper();
    let mut at = start;
    while at < end {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(base + (at - start)));
        let frame = PhysFrame::containing_address(PhysAddr::new(at));
        // SAFETY: the frame is inside a span `Inventory` proved firmware did not
        // claim as RAM, and `base` is a fresh window no other mapping uses.
        unsafe { space.map_to(page, frame, flags, &mut frames) }.map_err(map_error)?.flush();
        at += Size4KiB::SIZE;
    }
    state.cursor = frames.0.cursor();
    state.window = base + WINDOW_STRIDE;
    state.log.push(MappedRange::device(base, base + (end - start))).map_err(PlatformError::Mapping)?;
    Ok(base + (physical - start))
}

/// Page-table flags for `rights` and `cache`.
///
/// `PCD|PWT` selects PAT entry 3, which [`write_pat`] pins to uncacheable: the
/// memory type an MMIO window has to have for a register write to be a write.
fn leaf_flags(rights: Rights, cache: Cache) -> PageTableFlags {
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE;
    if rights.is_write() {
        flags |= PageTableFlags::WRITABLE;
    }
    if rights.is_execute() {
        flags.remove(PageTableFlags::NO_EXECUTE);
    }
    if cache == Cache::Device {
        flags |= PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH;
    }
    flags
}

/// The rights and memory type a live leaf grants.
///
/// The kernel never sets the PAT bit itself, so the memory type is the entry
/// selected by `PCD|PWT` alone. Only entry 3 is uncacheable, and everything
/// else reads back as write-back — the direction that makes an MMIO window
/// mapped with the wrong bits fail its audit rather than pass it.
fn protection(flags: PageTableFlags, size: u64) -> PageProtection {
    let index = u8::from(flags.contains(PageTableFlags::NO_CACHE)) << 1
        | u8::from(flags.contains(PageTableFlags::WRITE_THROUGH));
    let cache = if index == 3 { Cache::Device } else { Cache::WriteBack };
    let _ = size;
    PageProtection::new(
        flags.contains(PageTableFlags::PRESENT),
        flags.contains(PageTableFlags::WRITABLE),
        !flags.contains(PageTableFlags::NO_EXECUTE),
    )
    .cached(cache)
}

/// Programs IA32_PAT to the architectural reset configuration.
///
/// # Safety
///
/// Every mapping the caller relies on must select an entry whose type it still
/// expects afterwards; entries 0 and 3 keep their reset meaning, so leaves that
/// set neither bit or both are unaffected.
unsafe fn write_pat() {
    const IA32_PAT: u32 = 0x277;
    // WB, WT, UC-, UC, repeated in the high half: the reset value, written
    // rather than assumed so firmware cannot have moved UC off entry 3.
    const CONFIG: u64 = 0x0007_0406_0007_0406;
    // SAFETY: IA32_PAT is architectural on any CPU reporting PAT support, which
    // every x86_64 CPU does, and the value sets no reserved encoding.
    unsafe { crate::apic::write_msr(IA32_PAT, CONFIG) };
}

fn map_4k(
    space: &mut OffsetPageTable<'_>,
    frames: &mut X86Frames<'_>,
    virtual_address: u64,
    physical: u64,
    flags: PageTableFlags,
) -> Result<(), PlatformError> {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virtual_address));
    let frame = PhysFrame::containing_address(PhysAddr::new(physical));
    // SAFETY: the target table is not live yet, so no translation can be in use
    // while it changes, and every caller maps each virtual page exactly once.
    unsafe { space.map_to(page, frame, flags, frames) }.map_err(map_error)?.ignore();
    Ok(())
}

pub fn verify_owned_mapping(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    // Resume where `init` stopped: a fresh allocator over the same map would
    // hand back the frames the live tables are built from.
    let mut frames = X86Frames(BootFrameAllocator::resume(boot_info.memory_map(), state.cursor));
    let mut space = state.mapper();

    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let page = Page::containing_address(VirtAddr::new(TEST_PAGE));
    let mut mapping = OwnedPage::map(&mut space, &mut frames, page, permissions)?;
    mapping.write_and_verify(0x4d4f_4c54_5f57_585e)?;
    drop(mapping);
    // The probe leaf is gone again, so the audit that runs next sees exactly
    // the ranges `init` declared and nothing more.
    state.cursor = frames.0.cursor();
    Ok(())
}

pub fn verify_image_protection(_boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let space = state.mapper();
    let audit = state.log.audit();
    audit.cover(&MapperWalk { mapper: &space }).map_err(PlatformError::Mapping)?;
    // A full sweep of the live tables catches anything mapped that the kernel
    // never declared — the check the loader's tables could never have passed.
    sweep(state.offset, state.root, &audit)
}

/// Reads the APIC through the window `init` mapped and audits the result.
pub fn verify_device_window(_boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    // CPUID answers independently of the MMU, so an ID that matches proves the
    // window reaches the APIC and not some other frame that happens to be
    // readable — a stale direct-mapped alias would answer differently.
    let expected = (core::arch::x86_64::__cpuid(1).ebx >> 24) as u8;
    // SAFETY: `APIC_WINDOW` is mapped read/write and uncacheable to the APIC's
    // own frame; the ID register is a naturally aligned 32-bit MMIO location.
    let id = (unsafe { ((APIC_WINDOW + 0x20) as *const u32).read_volatile() } >> 24) as u8;
    if id != expected {
        return Err(PlatformError::InvalidHardware);
    }

    // The read alone proves only half the window. RISC-V drives its UART
    // through the mapping; do the same here on the one APIC register that is
    // writable without disturbing interrupt delivery — the task priority
    // register, which the kernel leaves at zero. Write a priority class, read
    // it back through the same window, then restore it.
    // SAFETY: `APIC_WINDOW + 0x80` is the naturally aligned 32-bit TPR of the
    // APIC frame this window maps read/write and uncacheable.
    let observed = unsafe {
        let tpr = (APIC_WINDOW + 0x80) as *mut u32;
        let previous = tpr.read_volatile();
        tpr.write_volatile(0x20);
        let observed = tpr.read_volatile();
        tpr.write_volatile(previous);
        observed
    };
    if observed & 0xff != 0x20 {
        return Err(PlatformError::InvalidHardware);
    }

    let space = state.mapper();
    let audit = state.log.audit();
    audit.cover(&MapperWalk { mapper: &space }).map_err(PlatformError::Mapping)?;
    sweep(state.offset, state.root, &audit)
}

impl Space {
    fn mapper(&self) -> OffsetPageTable<'static> {
        // SAFETY: the boot CPU is the only writer, `root` is this address
        // space's level-4 table, and `offset` direct-maps every table frame.
        unsafe {
            OffsetPageTable::new(
                &mut *table_pointer(self.offset, self.root),
                VirtAddr::new(self.offset),
            )
        }
    }
}

/// Hands every present leaf of the live tables to `audit`.
fn sweep(offset: u64, root: PhysFrame<Size4KiB>, audit: &Audit<'_>) -> Result<(), PlatformError> {
    walk(offset, root, 3, 0, audit)
}

fn walk(
    offset: u64,
    frame: PhysFrame<Size4KiB>,
    level: usize,
    base: u64,
    audit: &Audit<'_>,
) -> Result<(), PlatformError> {
    // SAFETY: `frame` holds a 512-entry page table, direct-mapped at `offset`.
    let table = unsafe { &*table_pointer(offset, frame) };
    let shift = 12 + 9 * level;
    for (index, entry) in table.iter().enumerate() {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let start = canonical(base | ((index as u64) << shift));
        let leaf = level == 0 || entry.flags().contains(PageTableFlags::HUGE_PAGE);
        if leaf {
            let size = 1u64 << shift;
            audit
                .accepts(Leaf::new(start, size, protection(entry.flags(), size)))
                .map_err(PlatformError::Mapping)?;
            continue;
        }
        let next = PhysFrame::containing_address(entry.addr());
        walk(offset, next, level - 1, start, audit)?;
    }
    Ok(())
}

/// Sign-extends bit 47 the way the hardware does, so a higher-half leaf's
/// address matches the range the kernel declared it under.
const fn canonical(address: u64) -> u64 {
    ((address << 16) as i64 >> 16) as u64
}

fn table_pointer(offset: u64, frame: PhysFrame<Size4KiB>) -> *mut PageTable {
    (offset + frame.start_address().as_u64()) as *mut PageTable
}

/// [`PageWalk`] over the live x86_64 tables.
struct MapperWalk<'m, 't> {
    mapper: &'m OffsetPageTable<'t>,
}

impl PageWalk for MapperWalk<'_, '_> {
    fn leaf(&self, address: u64) -> Option<Leaf> {
        // Report the actual leaf size, not always 4 KiB, so a 2 MiB huge page
        // across the image surfaces as a granularity error.
        let TranslateResult::Mapped { frame, flags, .. } =
            self.mapper.translate(VirtAddr::new(address))
        else {
            return None;
        };
        let size = frame.size();
        let start = address & !(size - 1);
        Some(Leaf::new(start, size, protection(flags, size)))
    }
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
        let frame = frames.allocate_frame().ok_or(out_of_frames())?;
        let rights = Rights::new(true, permissions.is_write(), permissions.is_execute())
            .map_err(PlatformError::Mapping)?;
        // SAFETY: TEST_PAGE is a dedicated, otherwise-unused virtual page and `frame` is a fresh
        // unique frame. W^X was validated by `Rights` before flags were constructed.
        unsafe { mapper.map_to(page, frame, leaf_flags(rights, Cache::WriteBack), frames) }
            .map_err(map_error)?
            .flush();
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

/// The shared [`molt_arch::align_down`] at this platform's page size.
fn align_down(address: u64) -> u64 {
    molt_arch::align_down(address, Size4KiB::SIZE)
}

/// The shared [`molt_arch::align_up`] at this platform's page size.
fn align_up(address: u64) -> Result<u64, PlatformError> {
    molt_arch::align_up(address, Size4KiB::SIZE).ok_or(address_error())
}

fn map_error(_error: MapToError<impl PageSize>) -> PlatformError {
    PlatformError::Mapping(MappingError::Backend)
}

fn address_error() -> PlatformError {
    PlatformError::Mapping(MappingError::InvalidAddress)
}

fn out_of_frames() -> PlatformError {
    PlatformError::Mapping(MappingError::OutOfFrames)
}

unsafe fn active_level_4_table(physical_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let physical = level_4_frame.start_address().as_u64();
    let virtual_address = physical_offset + physical;
    let pointer = virtual_address.as_mut_ptr();
    // SAFETY: the caller guarantees a complete direct map and unique access during early boot.
    unsafe { &mut *pointer }
}

/// Extends `end` to the end of the loader's contiguous mapping from `start`.
///
/// A reported image length covers the file image only; the loader maps the
/// zeroed sections beyond it, and those pages are as much the kernel as the
/// ones that came from the file.
fn mapped_end(live: &OffsetPageTable<'_>, start: u64, end: u64) -> Result<u64, PlatformError> {
    use x86_64::structures::paging::Translate;

    let mut end = align_up(end)?;
    while live.translate_addr(VirtAddr::new(end)).is_some() && end - start < IMAGE_LIMIT {
        end += Size4KiB::SIZE;
    }
    Ok(end)
}
