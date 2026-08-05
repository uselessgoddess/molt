//! Kernel-owned x86_64 translation tables.
//!
//! [`init`] maps usable RAM, clones the live image and boot windows, adds an
//! uncacheable APIC window, and loads `CR3`. Owning the exact declared set
//! enables both directions of [`Audit`] without inheriting loader mappings.

use core::cell::UnsafeCell;

use molt_arch::audit::{Audit, Contents, Declared, Leaf, MappedRange, PageWalk};
use molt_arch::memory::{Cache, Device, Inventory, Rights, Span};
use molt_arch::{
    BootInfo, FrameAllocator as BootFrameAllocator, FrameCursor, MapPermissions, MappingError,
    MemoryMap, Mmio, PageProtection, PlatformError, UsableRegions,
};
use x86_64::registers::control::{Cr3, Cr3Flags, Cr4, Cr4Flags};
use x86_64::structures::paging::mapper::{MapToError, TranslateResult};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size2MiB, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::{BOOT_INFO_BASE, BOOT_INFO_WINDOW, STACK_BASE, STACK_SIZE};

/// Maximum contiguous mapping scanned beyond the image's reported file length.
const IMAGE_LIMIT: u64 = 64 * 1024 * 1024;

const TEST_PAGE: u64 = 0x0000_5555_5555_0000;

const APIC_WINDOW: u64 = 0xffff_9200_0000_0000;

/// Where [`map_device`] puts the windows a driver asks for, and how far it may
/// go.
const DEVICE_REGION: u64 = 0xffff_9300_0000_0000;
const DEVICE_REGION_END: u64 = DEVICE_REGION + (1 << 30);

/// How many page-table frames are set aside for device mappings.
const DEVICE_TABLE_FRAMES: usize = 32;

/// How far up an application processor's first instruction may live: a core
/// coming out of reset starts at `vector << 12`, and the vector is one byte.
const TRAMPOLINE_LIMIT: u64 = 1 << 20;

type TableFrames = molt_arch::FramePool<DEVICE_TABLE_FRAMES>;

/// Declared ranges: the image, two boot windows, the APIC, and one entry per
/// usable RAM region — firmware maps are chatty, so this is generous.
type Log = Declared<64>;

struct Space {
    root: PhysFrame<Size4KiB>,
    offset: u64,
    cursor: FrameCursor,
    log: Log,
    /// Table frames reserved for windows mapped after the memory map is gone.
    pool: TableFrames,
    /// The next free device address; bumps forward and never back, so a window
    /// is never re-issued at an address a previous one used to hold.
    devices: u64,
    /// The low frame reserved for starting the other cores, if there was one.
    trampoline: Option<u64>,
}

/// The frame an application processor's first instruction runs from.
///
/// Identity-mapped as well as direct-mapped, because the core reaches it twice:
/// once in real mode by physical address, and once more with paging on, in the
/// instruction between loading `CR3` and jumping into the kernel's half of the
/// address space.
#[derive(Clone, Copy, Debug)]
pub struct Trampoline {
    physical: u64,
    page: *mut u8,
    root: u64,
}

impl Trampoline {
    /// What a startup interrupt names, which is this frame's number.
    pub fn physical(&self) -> u64 {
        self.physical
    }

    /// Where the boot core writes the frame, through the direct map.
    pub fn page(&self) -> *mut u8 {
        self.page
    }

    /// The tables the core comes up on, already built and live.
    pub fn root(&self) -> u64 {
        self.root
    }
}

/// The frame set aside for starting cores, or `None` on a machine that left no
/// usable memory under a megabyte.
pub fn trampoline() -> Option<Trampoline> {
    let state = active().ok()?;
    let physical = state.trampoline?;
    Some(Trampoline {
        physical,
        page: (state.offset + physical) as *mut u8,
        root: state.root.start_address().as_u64(),
    })
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

/// What this machine's translation hardware can do, read out of the registers
/// that already say so.
///
/// The address width is whatever paging mode the loader left on: `CR4.LA57`
/// selects five levels and 57 bits, and its absence is four levels and 48. The
/// kernel does not switch modes to find out, because unlike RISC-V's `satp`
/// probe, changing it means rebuilding the tree the running code is translating
/// through.
///
/// The tag width is a capability, not a state: `CPUID.01H:ECX[17]` says the
/// machine has PCIDs, and enabling them is `CR4.PCIDE`, which this kernel does
/// not do until domains exist. Twelve bits is what it will get when it does; a
/// machine without the bit gets zero, and pays a flush per view switch.
pub fn widths() -> molt_arch::va::Widths {
    let address = if Cr4::read().contains(Cr4Flags::L5_PAGING) { 57 } else { 48 };
    let features = core::arch::x86_64::__cpuid(1);
    let asid = if features.ecx & (1 << 17) != 0 { 12 } else { 0 };
    molt_arch::va::Widths::new(address, asid)
}

/// A cursor past the RAM the address space is already built out of.
///
/// Boot drained tables and cloned windows up to this point; a driver resumes a
/// [`BootFrameAllocator`] here to back DMA out of frames no live mapping owns.
pub fn free_frames() -> Option<FrameCursor> {
    active().ok().map(|state| state.cursor)
}

/// Hands out `count` frames of that RAM and moves the cursor past them.
///
/// The direct map already covers every usable region, so the caller reaches the
/// span at `offset + start` without mapping anything.
///
/// The move is the point and it is permanent: the frames are the caller's for
/// good, no later claim can be handed the same ones, and a [`FrameCursor`] read
/// before this call is stale.
pub fn claim_ram(map: &dyn MemoryMap, count: u64) -> Result<Span, PlatformError> {
    let state = active()?;
    let mut frames = BootFrameAllocator::resume(map, state.cursor);
    let span = frames.run(count)?;
    state.cursor = frames.cursor();
    Ok(span)
}

/// Builds the kernel address space and returns its local APIC window.
pub fn init(boot_info: &BootInfo<'_>) -> Result<u64, PlatformError> {
    let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
    let image = boot_info.kernel_image().ok_or(PlatformError::Mapping(MappingError::Unmapped))?;
    let map = boot_info.memory_map();
    let mut frames = X86Frames(BootFrameAllocator::new(map));

    // The lowest usable frame is the only candidate for the trampoline, and it
    // is taken first so nothing else can be handed it. A machine that left none
    // under a megabyte keeps the frame as a table instead and boots one core.
    let first = frames.allocate_frame().ok_or(out_of_frames())?;
    let low = first.start_address().as_u64() < TRAMPOLINE_LIMIT;
    let root = match low {
        true => frames.allocate_frame().ok_or(out_of_frames())?,
        false => first,
    };
    let trampoline = low.then(|| first.start_address().as_u64());
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
    // Preserve the live virtual addresses and observed rights across the CR3 switch.
    // The reported file length excludes mapped zero-fill sections such as `.bss`.
    let image_end = mapped_end(&live, image.start(), image.end())?;
    clone(&mut space, &mut frames, &mut log, &live, image.start(), image_end, Contents::Image)?;
    // Include the stack's upper page while preserving the unmapped guard page.
    let stack_end = STACK_BASE + STACK_SIZE + 2 * Size4KiB::SIZE;
    clone(&mut space, &mut frames, &mut log, &live, STACK_BASE, stack_end, Contents::Ram)?;
    let boot_info_end = BOOT_INFO_BASE + BOOT_INFO_WINDOW;
    clone(&mut space, &mut frames, &mut log, &live, BOOT_INFO_BASE, boot_info_end, Contents::Ram)?;
    device_window(&mut space, &mut frames, &mut log, map, crate::apic::APIC_MMIO, APIC_WINDOW)?;
    if let Some(physical) = trampoline {
        // Identity, because a core that has just loaded `CR3` is still fetching
        // by physical address. Executable and not writable: the boot core writes
        // the code through the direct map, the starting core only runs it.
        let flags = leaf_flags(Rights::READ_EXECUTE, Cache::WriteBack);
        map_4k(&mut space, &mut frames, physical, physical, flags)?;
        log.push(MappedRange::image(physical, physical + Size4KiB::SIZE))
            .map_err(PlatformError::Mapping)?;
    }

    // SAFETY: every x86_64 CPU has PAT, and the reset layout preserves active entries.
    unsafe { write_pat() };

    // Drained here, while the map is still borrowable, and out of the same
    // allocator every table frame came from, so a device window mapped later
    // cannot be handed a frame this address space is already built out of.
    let mut pool = TableFrames::empty();
    pool.fill(&mut frames.0);

    let cursor = frames.0.cursor();
    // SAFETY: the executing code, its stack, the boot info, the tables reached
    // through `offset`, and the APIC window are all present in `root`, so
    // translation can switch in place, and `Cr3Flags::empty()` leaves the table
    // walk write-back cacheable.
    unsafe { Cr3::write(root, Cr3Flags::empty()) };
    // SAFETY: same reasoning as `active`; this runs once on the boot CPU.
    unsafe {
        *ACTIVE.0.get() =
            Some(Space { root, offset, cursor, log, pool, devices: DEVICE_REGION, trampoline })
    };
    Ok(APIC_WINDOW)
}

/// Direct-maps only firmware-usable RAM.
fn direct_map(
    space: &mut OffsetPageTable<'_>,
    frames: &mut X86Frames<'_>,
    log: &mut Log,
    map: &dyn molt_arch::MemoryMap,
    offset: u64,
) -> Result<(), PlatformError> {
    for region in UsableRegions::above(map, 0) {
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

/// Clones mapped pages and their live rights, preserving holes.
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
    let window = Inventory::new(map).device(span).map_err(|_| address_error())?;
    let (rights, cache) = window.mapping(Rights::READ_WRITE).map_err(PlatformError::Mapping)?;
    map_4k(space, frames, virtual_address, window.span().start(), leaf_flags(rights, cache))?;
    log.push(MappedRange::device(virtual_address, virtual_address + Size4KiB::SIZE))
        .map_err(PlatformError::Mapping)
}

/// Maps `window` into the live address space and hands back its registers.
///
/// Unlike [`device_window`], this edits tables the CPU is already walking, so
/// each leaf is flushed as it is created. Frames come from the pool [`init`]
/// drained; a fresh allocator would reissue frames the live tables own.
pub fn map_device(window: Device, rights: Rights) -> Result<Mmio<'static>, MappingError> {
    let state = active().map_err(|_| MappingError::Unmapped)?;
    let (rights, cache) = window.mapping(rights)?;
    let flags = leaf_flags(rights, cache);

    let span = window.span();
    let bytes = span.bytes();
    let base = state.devices;
    let end = base.checked_add(bytes).ok_or(MappingError::InvalidAddress)?;
    if end > DEVICE_REGION_END {
        return Err(MappingError::OutOfFrames);
    }

    let mut space = state.mapper();
    {
        let mut frames = PoolFrames(&mut state.pool);
        let mut address = 0;
        while address < bytes {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(base + address));
            if !matches!(space.translate(page.start_address()), TranslateResult::NotMapped) {
                return Err(MappingError::Unexpected);
            }
            let frame =
                PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(span.start() + address));
            // SAFETY: `Inventory::device` proved this is not RAM, the page was
            // just proven unmapped, and the flags carry no execute permission.
            unsafe { space.map_to(page, frame, flags, &mut frames) }
                .map_err(|_| MappingError::Backend)?
                .flush();
            address += molt_arch::FRAME_SIZE;
        }
    }

    state.log.push(MappedRange::device(base, end))?;
    state.devices = end.next_multiple_of(2 * 1024 * 1024);
    // SAFETY: every frame of `span` was just mapped at `base`, uncached and
    // non-executable, and never unmapped. The bump cursor guarantees no second
    // window over the same virtual range.
    Ok(unsafe { Mmio::new(base as *mut u8, bytes) })
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

/// Decodes rights and the PAT entry selected by `PCD|PWT`.
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
    // Architectural reset layout: WB, WT, UC-, UC in each half.
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
    let mut frames = X86Frames(BootFrameAllocator::resume(boot_info.memory_map(), state.cursor));
    let mut space = state.mapper();

    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let page = Page::containing_address(VirtAddr::new(TEST_PAGE));
    let mut mapping = OwnedPage::map(&mut space, &mut frames, page, permissions)?;
    mapping.write_and_verify(0x4d4f_4c54_5f57_585e)?;
    drop(mapping);
    // Remove the probe before auditing the declared mappings.
    state.cursor = frames.0.cursor();
    Ok(())
}

pub fn verify_image_protection(_boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let space = state.mapper();
    let audit = state.log.audit();
    audit.cover(&MapperWalk { mapper: &space }).map_err(PlatformError::Mapping)?;
    sweep(state.offset, state.root, &audit)
}

/// Reads the APIC through the window `init` mapped and audits the result.
pub fn verify_device_window(_boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    // Compare against CPUID to detect a readable mapping of the wrong frame.
    let expected = (core::arch::x86_64::__cpuid(1).ebx >> 24) as u8;
    // SAFETY: `APIC_WINDOW` is mapped read/write and uncacheable to the APIC's
    // own frame; the ID register is a naturally aligned 32-bit MMIO location.
    let id = (unsafe { ((APIC_WINDOW + 0x20) as *const u32).read_volatile() } >> 24) as u8;
    if id != expected {
        return Err(PlatformError::InvalidHardware);
    }

    // Round-trip the task-priority register without disturbing interrupt delivery.
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

/// The largest leaf covering RAM, found by walking the live tables.
///
/// Every address of every direct-mapped range is accounted for, one leaf at a
/// time, so the answer cannot come from a lucky probe: a hole inside a range
/// `direct_map` claims to have mapped stops the walk instead of being stepped
/// over on the way to the bigger leaf next door.
pub fn largest_ram_leaf(boot_info: &BootInfo<'_>) -> Result<Leaf, PlatformError> {
    let state = active()?;
    let space = state.mapper();
    let walk = MapperWalk { mapper: &space };

    let mut largest: Option<Leaf> = None;
    for region in UsableRegions::above(boot_info.memory_map(), 0) {
        // Same bounds `direct_map` used; anything outside them was never mapped.
        let (start, end) = (align_up(region.start())?, align_down(region.end()));
        let mut address = state.offset + start;
        while address < state.offset + end {
            let leaf = walk.leaf(address).ok_or(MappingError::Unmapped)?;
            if largest.is_none_or(|it| it.size() < leaf.size()) {
                largest = Some(leaf);
            }
            address = leaf.end().max(address + 1);
        }
    }
    largest.ok_or(PlatformError::Mapping(MappingError::Unmapped))
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

const fn canonical(address: u64) -> u64 {
    ((address << 16) as i64 >> 16) as u64
}

fn table_pointer(offset: u64, frame: PhysFrame<Size4KiB>) -> *mut PageTable {
    (offset + frame.start_address().as_u64()) as *mut PageTable
}

struct MapperWalk<'m, 't> {
    mapper: &'m OffsetPageTable<'t>,
}

impl PageWalk for MapperWalk<'_, '_> {
    fn leaf(&self, address: u64) -> Option<Leaf> {
        // Preserve huge-page size so the audit sees crossed boundaries.
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

/// The table frames [`init`] set aside, adapted for the mapper.
struct PoolFrames<'pool>(&'pool mut TableFrames);

// SAFETY: the pool was drained from a `BootFrameAllocator` that hands out each
// frame at most once, and hands each one on at most once in turn.
unsafe impl FrameAllocator<Size4KiB> for PoolFrames<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.0.allocate().map(|frame| PhysFrame::containing_address(PhysAddr::new(frame.start())))
    }
}

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
        // SAFETY: TEST_PAGE is a dedicated, otherwise-unused virtual page mapped to a fresh unique
        // frame, with W^X validated by `Rights` before the flags were constructed.
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

fn align_down(address: u64) -> u64 {
    molt_arch::align_down(address, Size4KiB::SIZE)
}

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

/// Returns the active level-4 table through its physical direct map.
///
/// # Safety
///
/// `physical_offset` must map the active root, with unique access during boot.
unsafe fn active_level_4_table(physical_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let physical = level_4_frame.start_address().as_u64();
    let virtual_address = physical_offset + physical;
    let pointer = virtual_address.as_mut_ptr();
    // SAFETY: the caller guarantees a complete direct map and unique access during early boot.
    unsafe { &mut *pointer }
}

/// Extends the file-backed image end through its contiguous zero-fill mapping.
fn mapped_end(live: &OffsetPageTable<'_>, start: u64, end: u64) -> Result<u64, PlatformError> {
    use x86_64::structures::paging::Translate;

    let mut end = align_up(end)?;
    while live.translate_addr(VirtAddr::new(end)).is_some() && end - start < IMAGE_LIMIT {
        end += Size4KiB::SIZE;
    }
    Ok(end)
}
