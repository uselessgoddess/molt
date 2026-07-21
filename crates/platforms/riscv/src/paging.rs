//! Sv39 boot address space that maps the kernel image one section at a time.
//!
//! The earlier version identity-mapped all of RAM with a single RWX gigapage.
//! That kept the kernel running, but it also meant `.text` was writable and
//! `.data` was executable, so the W^X contract `MapPermissions` enforces on
//! x86_64 held nowhere on RISC-V. [`init`] instead walks the linker-exported
//! section bounds and gives each span exactly the rights it needs — `.text`
//! read/execute, `.rodata` read-only, `.data`/`.bss`/boot stack read/write up
//! to `__kernel_end`, and only firmware-declared usable RAM above that as free
//! RAM. Reserved regions, firmware, and MMIO holes get no mapping at all.
//!
//! Two checks are built on top of it. [`verify_owned_mapping`] maps one private
//! page with [`MapPermissions`]-derived flags and round-trips a value through
//! the MMU. [`verify_image_protection`] reads the live page tables back and
//! walks every declared range, confirming each page holds the rights it was
//! mapped with — the check that would have failed against the old gigapage,
//! and that a probe of a handful of addresses would have missed.

use core::arch::asm;
use core::cell::UnsafeCell;

use molt_arch::audit::{Audit, Declared, Leaf, MappedRange, PageWalk};
use molt_arch::memory::{Cache, Inventory, Kind, Rights, Span};
use molt_arch::{
    BootInfo, FrameAllocator, FrameCursor, ImageSection, MapPermissions, MappingError,
    PageProtection, PhysicalFrame, PlatformError, UsableRegions,
};

/// Sv39 page-table entry flags.
const PTE_V: u64 = 1 << 0; // valid
const PTE_R: u64 = 1 << 1; // readable
const PTE_W: u64 = 1 << 2; // writable
const PTE_X: u64 = 1 << 3; // executable
const PTE_A: u64 = 1 << 6; // accessed
const PTE_D: u64 = 1 << 7; // dirty

/// Permission bits: a non-zero mask is what distinguishes a leaf from a pointer.
const PTE_RWX: u64 = PTE_R | PTE_W | PTE_X;

/// `satp.MODE` value selecting three-level Sv39 translation.
const SATP_MODE_SV39: u64 = 8 << 60;

/// Bytes spanned by a level-0 leaf.
const PAGE_4K: usize = 4096;
/// Bytes spanned by a level-1 leaf (a megapage).
const PAGE_2M: usize = 2 * 1024 * 1024;

/// A private virtual address, outside the mapped image, for the probe page.
const PROBE_VA: usize = 0x2000_0000;
/// Distinctive payload written through the probe mapping ("MOLT_WX").
const PROBE_VALUE: u64 = 0x004d_4f4c_545f_5758;

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

/// The largest number of usable free-RAM ranges the audit stores inline.
///
/// The QEMU `virt` build exposes one contiguous span, and no plausible RISC-V
/// board grows this to double digits; growing it here is a one-line change.
const MAX_RAM_RANGES: usize = 8;

/// How many windows the kernel may open onto devices after boot: the UART the
/// mapping check writes through, configuration space, and room for the register
/// blocks a driver asks for.
const MAX_DEVICE_WINDOWS: usize = 8;

/// Every mapped range the boot address space declares: three image sections,
/// the free-RAM regions, and the device windows opened on top of them.
type MappingLog = Declared<{ 3 + MAX_RAM_RANGES + MAX_DEVICE_WINDOWS }>;

/// Where windows onto devices are laid out, and how far apart.
///
/// Sixteen gigabytes up is past anything this board puts in physical memory,
/// which is the point: a window's address is the kernel's to choose, and one
/// that cannot be confused with the address it maps is one no driver can reach
/// by assuming identity. Windows are handed out upwards from here with a
/// megapage of unmapped space between them, so a driver that walks off the end
/// of its registers faults instead of landing in another device.
const DEVICE_WINDOW: usize = 0x4_0000_0000;
const WINDOW_GAP: usize = PAGE_2M;

/// Address of a linker-defined symbol.
macro_rules! bound {
    ($symbol:ident) => {
        (&raw const $symbol) as usize
    };
}

/// The boot address space, kept so later probes can extend it.
struct BootPaging {
    root: *mut u64,
    /// Where frame allocation stopped, so a probe does not reissue a table frame.
    cursor: FrameCursor,
    /// Where the next device window goes.
    window: usize,
    /// Every mapped range, in the order [`init`] built them.
    log: MappingLog,
}

struct Active(UnsafeCell<Option<BootPaging>>);

// SAFETY: the boot address space is built and used on the single boot hart
// before any other hart is started, so there is no concurrent access to share.
unsafe impl Sync for Active {}

static ACTIVE: Active = Active(UnsafeCell::new(None));

/// Borrows the boot address space, or reports that [`init`] has not run.
fn active() -> Result<&'static mut BootPaging, PlatformError> {
    // SAFETY: single boot hart, traps do not touch this cell, and the returned
    // borrow is confined to one call.
    unsafe { &mut *ACTIVE.0.get() }.as_mut().ok_or(PlatformError::Mapping(MappingError::Unmapped))
}

/// Builds the per-section boot address space and enables Sv39 translation.
///
/// Free RAM is mapped only for firmware-reported usable regions above the
/// image, so reserved ranges, firmware, and MMIO holes get no S-mode mapping
/// at all. The paging tables and the frame allocator see the same set of
/// pages because [`UsableRegions`] and [`FrameAllocator::above`] share a floor.
pub fn init(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let kernel_end = bound!(__kernel_end) as u64;
    let mut frames = FrameAllocator::above(boot_info.memory_map(), kernel_end);
    let root = alloc_table(&mut frames)?;

    let mut log = MappingLog::new();
    // Identity mappings throughout: `satp` is enabled from code that is running
    // at its physical address, so any virtual-to-physical shift would fault on
    // the instruction right after the `csrw`.
    //
    // The OpenSBI region below the payload address is deliberately left out: it
    // is M-mode firmware, and nothing in S-mode has any business reaching it.
    map_section(
        root,
        &mut frames,
        &mut log,
        ImageSection::Text,
        bound!(__text_start),
        bound!(__text_end),
    )?;
    map_section(
        root,
        &mut frames,
        &mut log,
        ImageSection::Rodata,
        bound!(__rodata_start),
        bound!(__rodata_end),
    )?;
    // `.data`, `.bss`, and the boot stack are one writable span that stops at
    // `__kernel_end`. Free RAM above it is mapped separately, once per usable
    // region: mapping through firmware or MMIO holes is exactly what an audit
    // would flag next.
    map_section(
        root,
        &mut frames,
        &mut log,
        ImageSection::Data,
        bound!(__data_start),
        bound!(__kernel_end),
    )?;

    for range in UsableRegions::above(boot_info.memory_map(), kernel_end) {
        let start = usize::try_from(range.start())
            .map_err(|_| PlatformError::Mapping(MappingError::InvalidAddress))?;
        let end = usize::try_from(range.end())
            .map_err(|_| PlatformError::Mapping(MappingError::InvalidAddress))?;
        map_range(root, &mut frames, start, end, PTE_R | PTE_W, Granularity::LargeOk)?;
        log.push(MappedRange::ram(range.start(), range.end())).map_err(PlatformError::Mapping)?;
    }

    let cursor = frames.cursor();
    // SAFETY: every address the kernel executes from, reads, or writes — code,
    // constants, stack, and the page tables themselves — was just identity
    // mapped, so translation can be switched on in place.
    unsafe {
        enable_sv39(root as u64);
    }
    // SAFETY: same reasoning as `active`; this runs once on the boot hart.
    unsafe {
        *ACTIVE.0.get() = Some(BootPaging { root, cursor, window: DEVICE_WINDOW, log });
    }
    Ok(())
}

fn map_section(
    root: *mut u64,
    frames: &mut FrameAllocator<'_>,
    log: &mut MappingLog,
    section: ImageSection,
    start: usize,
    end: usize,
) -> Result<(), PlatformError> {
    let flags = match section {
        ImageSection::Text => PTE_R | PTE_X,
        ImageSection::Rodata => PTE_R,
        ImageSection::Data => PTE_R | PTE_W,
    };
    map_range(root, frames, start, end, flags, Granularity::Small)?;
    // The audit walks the range page by page and needs its aligned bounds, so
    // it agrees with whatever [`map_range`] actually gave the MMU.
    let aligned_start = align_down(start, PAGE_4K) as u64;
    let aligned_end =
        align_up(end, PAGE_4K).ok_or(PlatformError::Mapping(MappingError::InvalidAddress))? as u64;
    log.push(MappedRange::section(section, aligned_start, aligned_end))
        .map_err(PlatformError::Mapping)
}

pub fn verify_owned_mapping(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    // Resume where `init` stopped: a fresh allocator over the same map would
    // hand back the frames the live page tables are built from.
    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);

    // Derive the probe's leaf flags from W^X-checked permissions. A writable
    // RISC-V leaf must also be readable, and must never be executable here.
    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let mut leaf = PTE_A | PTE_D;
    if permissions.is_write() {
        leaf |= PTE_R | PTE_W;
    }
    if permissions.is_execute() {
        leaf |= PTE_X;
    }

    let probe = alloc_frame(&mut frames)?;
    map_leaf(state.root, &mut frames, PROBE_VA, probe, leaf, 0)?;
    state.cursor = frames.cursor();
    // SAFETY: the new leaf is visible in memory; the fence retires any negative
    // caching of `PROBE_VA` from before it existed.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }

    let pointer = PROBE_VA as *mut u64;
    // SAFETY: `PROBE_VA` is now mapped present, readable, and writable to a
    // uniquely owned frame; the access is naturally aligned and volatile.
    let outcome = unsafe {
        pointer.write_volatile(PROBE_VALUE);
        if pointer.read_volatile() != PROBE_VALUE {
            Err(PlatformError::Mapping(MappingError::Backend))
        } else {
            Ok(())
        }
    };
    // Clear the probe leaf whether the round-trip passed or not, so the audit
    // that runs next sees exactly the ranges [`init`] declared, nothing more.
    // SAFETY: nothing else references `PROBE_VA` after this scope, and the
    // fence retires the stale translation before another access can hit it.
    unsafe {
        clear_leaf(state.root, PROBE_VA);
        asm!("sfence.vma", options(nostack));
    }
    outcome
}

/// Clears the level-0 PTE that translates `va`, if one exists.
///
/// # Safety
///
/// The caller must ensure no other thread holds a cached translation for `va`
/// after this returns; the boot hart is single-threaded, so a following
/// `sfence.vma` on it is enough.
unsafe fn clear_leaf(root: *mut u64, va: usize) {
    let mut table = root;
    for level in (1..=2).rev() {
        // SAFETY: `table` addresses a 512-entry table frame, identity mapped
        // by `init`, and the index is masked to nine bits.
        let entry = unsafe { *table.add(index(va, level)) };
        if entry & PTE_V == 0 || entry & PTE_RWX != 0 {
            return;
        }
        table = ((entry >> 10) << 12) as *mut u64;
    }
    // SAFETY: `table` addresses the level-0 table frame covering `va`.
    unsafe {
        table.add(index(va, 0)).write(0);
    }
}

pub fn verify_image_protection(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let inventory = Inventory::new(boot_info.memory_map());
    let walk = TableWalk { root: state.root, inventory: &inventory };
    state.log.audit().cover(&walk).map_err(PlatformError::Mapping)?;
    // A full sweep of the live tables catches anything mapped that the kernel
    // never declared — a stray writable range through firmware or MMIO would
    // fail here rather than escape the outward-only check above.
    walk_leaves(state.root, &state.log.audit(), &inventory)
}

/// The QEMU `virt` board's NS16550A transmitter, the first mapped device.
const UART_MMIO: u64 = 0x1000_0000;
/// Where that window lives in the kernel's address space — deliberately not
/// its physical address, so nothing can reach the UART by assuming identity.
const UART_WINDOW: usize = 0x3000_0000;
/// Transmitter holding register, and the line status whose bit 5 means "empty".
const UART_THR: usize = 0;
const UART_LSR: usize = 5;
const UART_LSR_THRE: u8 = 1 << 5;

/// Maps the UART through [`Inventory::device`] and writes a line through it.
///
/// Everything before this reached hardware either through an identity mapping
/// or through firmware, so this is the first evidence that a device is
/// reachable *because* the kernel mapped it, with the rights and the memory
/// type its window says it may have, and that the audit sees it that way too.
pub fn verify_device_window(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let inventory = Inventory::new(boot_info.memory_map());
    let span = Span::frames(UART_MMIO, 1).map_err(|_| address_error())?;
    // Refuses anything firmware claimed as RAM, so a mistyped constant cannot
    // become a device mapping over the kernel's own memory.
    let window = inventory.device(span).map_err(|_| address_error())?;
    let (rights, cache) = window.mapping(Rights::READ_WRITE).map_err(PlatformError::Mapping)?;
    debug_assert_eq!(cache, Cache::Device);

    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);
    map_leaf(state.root, &mut frames, UART_WINDOW, window.span().start(), leaf_flags(rights), 0)?;
    state.cursor = frames.cursor();
    state
        .log
        .push(MappedRange::device(UART_WINDOW as u64, UART_WINDOW as u64 + PAGE_4K as u64))
        .map_err(PlatformError::Mapping)?;
    // SAFETY: the new leaf is in memory; the fence retires any negative caching
    // of `UART_WINDOW` from before it existed.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }

    for byte in b"MOLT_UART_WINDOW: ns16550a\n" {
        // SAFETY: the window is mapped read/write to the UART's own frame, and
        // both registers are single bytes at fixed offsets within it.
        unsafe {
            while (UART_WINDOW as *const u8).add(UART_LSR).read_volatile() & UART_LSR_THRE == 0 {}
            (UART_WINDOW as *mut u8).add(UART_THR).write_volatile(*byte);
        }
    }

    // Re-audit with the window declared: `cover` proves the leaf is uncacheable
    // and unexecutable, and the sweep proves nothing else appeared with it.
    let walk = TableWalk { root: state.root, inventory: &inventory };
    state.log.audit().cover(&walk).map_err(PlatformError::Mapping)?;
    walk_leaves(state.root, &state.log.audit(), &inventory)
}

/// Maps `[base, base + bytes)` of device memory and says where it landed.
///
/// This is [`verify_device_window`] with the address it writes through no
/// longer a constant: the caller knows which registers it wants and nothing
/// else, and the window it gets back is one the audit has already accepted.
/// Every window goes through [`Inventory::device`] first, so a base that is
/// really RAM — a misread device tree, a driver's arithmetic — is refused here
/// rather than mapped uncacheable over the kernel's own memory.
///
/// The physical base's offset within a megapage is carried into the virtual
/// address, which is what lets a large aperture be mapped with megapages
/// instead of tens of thousands of small leaves.
pub fn open_window(
    boot_info: &BootInfo<'_>,
    base: u64,
    bytes: u64,
) -> Result<usize, PlatformError> {
    let state = active()?;
    let inventory = Inventory::new(boot_info.memory_map());
    let start = molt_arch::align_down(base, PAGE_4K as u64);
    let end = molt_arch::align_up(base.checked_add(bytes).ok_or(address_error())?, PAGE_4K as u64)
        .ok_or(address_error())?;
    let span = Span::new(start, end).map_err(|_| address_error())?;
    let window = inventory.device(span).map_err(|_| address_error())?;
    let (rights, cache) = window.mapping(Rights::READ_WRITE).map_err(PlatformError::Mapping)?;
    debug_assert_eq!(cache, Cache::Device);

    let len = usize::try_from(end - start).map_err(|_| address_error())?;
    let va = state.window + (start as usize % PAGE_2M);
    // The next window starts a megapage past the end of this one, rounded up so
    // its own offset arithmetic starts from a megapage boundary again.
    state.window = align_up(va + len, PAGE_2M).ok_or(address_error())? + WINDOW_GAP;

    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);
    let flags = leaf_flags(rights);
    let mut mapped = 0;
    while mapped < len {
        let (va, pa) = (va + mapped, start + mapped as u64);
        let large = va % PAGE_2M == 0 && pa % PAGE_2M as u64 == 0 && len - mapped >= PAGE_2M;
        map_leaf(state.root, &mut frames, va, pa, flags, large as usize)?;
        mapped += if large { PAGE_2M } else { PAGE_4K };
    }
    state.cursor = frames.cursor();
    state
        .log
        .push(MappedRange::device(va as u64, (va + len) as u64))
        .map_err(PlatformError::Mapping)?;
    // SAFETY: the new leaves are in memory; the fence retires any negative
    // caching of the window from before it existed.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }

    // The window is not handed out until the audit has seen it: `cover` proves
    // the leaves are uncacheable and unexecutable, and the sweep proves nothing
    // else appeared alongside them.
    let walk = TableWalk { root: state.root, inventory: &inventory };
    state.log.audit().cover(&walk).map_err(PlatformError::Mapping)?;
    walk_leaves(state.root, &state.log.audit(), &inventory)?;
    Ok(va)
}

/// Sv39 leaf flags for `rights`, with access and dirty pre-set.
fn leaf_flags(rights: Rights) -> u64 {
    let mut flags = PTE_A;
    if rights.is_read() {
        flags |= PTE_R;
    }
    if rights.is_write() {
        flags |= PTE_R | PTE_W | PTE_D;
    }
    if rights.is_execute() {
        flags |= PTE_X;
    }
    flags
}

fn address_error() -> PlatformError {
    PlatformError::Mapping(MappingError::InvalidAddress)
}

/// Walks the live translation tables and hands every present leaf to `audit`.
fn walk_leaves(
    root: *const u64,
    audit: &Audit<'_>,
    inventory: &Inventory<'_>,
) -> Result<(), PlatformError> {
    walk_table(root, 2, 0, audit, inventory)
}

fn walk_table(
    table: *const u64,
    level: usize,
    base: u64,
    audit: &Audit<'_>,
    inventory: &Inventory<'_>,
) -> Result<(), PlatformError> {
    let span_bits = 12 + 9 * level;
    for i in 0..512u64 {
        // SAFETY: `table` points at a 512-entry table frame, identity mapped
        // read/write by `init`, and the offset is masked to nine bits.
        let entry = unsafe { *table.add(i as usize) };
        if entry & PTE_V == 0 {
            continue;
        }
        let start = base | (i << span_bits);
        if entry & PTE_RWX != 0 {
            let size = 1u64 << span_bits;
            audit
                .accepts(Leaf::new(start, size, protection(entry, inventory)))
                .map_err(PlatformError::Mapping)?;
            continue;
        }
        // A pointer entry — walk into the child table it names.
        if level == 0 {
            return Err(PlatformError::Mapping(MappingError::Backend));
        }
        let next = ((entry >> 10) << 12) as *const u64;
        walk_table(next, level - 1, start, audit, inventory)?;
    }
    Ok(())
}

/// The rights and memory type a live Sv39 leaf grants.
///
/// Sv39 has no memory-type bits: `Svpbmt` adds them, it is an extension the
/// QEMU `virt` board's `rv64` CPU does not implement, and S-mode cannot even
/// ask without parsing the device tree. So the memory type of a leaf here is
/// not a property of the entry at all — it is the PMA of the physical address
/// the entry names, fixed by the platform. Classifying that address through
/// [`Inventory`] therefore reports what the hardware actually does: RAM and
/// image frames are write-back — as is firmware-reserved RAM — and anything in
/// a hole of the firmware map is
/// I/O-ordered. When `Svpbmt` is present it becomes an override on top of this
/// answer — a PBMT field of zero still means "whatever the PMA says" — so the
/// classification stays correct and gains a bit to read instead.
fn protection(entry: u64, inventory: &Inventory<'_>) -> PageProtection {
    let physical = (entry >> 10) << 12;
    let cache = match inventory.kind(physical) {
        Kind::Ram | Kind::Image | Kind::Reserved => Cache::WriteBack,
        Kind::Device => Cache::Device,
    };
    PageProtection::new(entry & PTE_R != 0, entry & PTE_W != 0, entry & PTE_X != 0).cached(cache)
}

/// [`PageWalk`] over the live Sv39 tables the kernel built.
struct TableWalk<'i> {
    root: *const u64,
    inventory: &'i Inventory<'i>,
}

impl PageWalk for TableWalk<'_> {
    fn leaf(&self, address: u64) -> Option<Leaf> {
        let va = usize::try_from(address).ok()?;
        let mut table = self.root;
        let mut level = 2;
        loop {
            // SAFETY: `table` points at a 512-entry Sv39 table frame — the
            // root by construction, or a child table `init` mapped read-only
            // through the identity map. The index is masked to nine bits.
            let entry = unsafe { *table.add(index(va, level)) };
            if entry & PTE_V == 0 {
                return None;
            }
            if entry & PTE_RWX != 0 {
                let span = 1u64 << (12 + 9 * level);
                let start = address & !(span - 1);
                return Some(Leaf::new(start, span, protection(entry, self.inventory)));
            }
            if level == 0 {
                return None;
            }
            table = ((entry >> 10) << 12) as *const u64;
            level -= 1;
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Granularity {
    Small,
    LargeOk,
}

fn map_range(
    root: *mut u64,
    frames: &mut FrameAllocator<'_>,
    start: usize,
    end: usize,
    rights: u64,
    granularity: Granularity,
) -> Result<(), PlatformError> {
    let mut flags = rights | PTE_A;
    if rights & PTE_W != 0 {
        flags |= PTE_D;
    }
    let mut va = align_down(start, PAGE_4K);
    let end = align_up(end, PAGE_4K).ok_or(PlatformError::Mapping(MappingError::InvalidAddress))?;
    while va < end {
        let level = (granularity == Granularity::LargeOk
            && va % PAGE_2M == 0
            && end - va >= PAGE_2M) as usize;
        map_leaf(root, frames, va, va as u64, flags, level)?;
        va += if level == 1 { PAGE_2M } else { PAGE_4K };
    }
    Ok(())
}

fn map_leaf(
    root: *mut u64,
    frames: &mut FrameAllocator<'_>,
    va: usize,
    pa: u64,
    flags: u64,
    level: usize,
) -> Result<(), PlatformError> {
    let mut table = root;
    for above in ((level + 1)..=2).rev() {
        // SAFETY: `table` always points at a valid 512-entry table frame.
        let entry = unsafe { &mut *table.add(index(va, above)) };
        if *entry & PTE_V == 0 {
            let next = alloc_table(frames)?;
            *entry = pte(next as u64, 0); // a pointer entry clears R/W/X.
        } else if *entry & PTE_RWX != 0 {
            // Splitting a live leaf is not implemented; ranges are mapped once.
            return Err(PlatformError::Mapping(MappingError::Backend));
        }
        table = ((*entry >> 10) << 12) as *mut u64;
    }
    // SAFETY: `table` is the level-`level` table covering `va`.
    unsafe {
        table.add(index(va, level)).write(pte(pa, flags));
    }
    Ok(())
}

/// Index into the level-`level` table for `va`.
fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * level)) & 0x1ff
}

/// The shared [`molt_arch::align_down`] on this platform's address width.
fn align_down(value: usize, alignment: usize) -> usize {
    molt_arch::align_down(value as u64, alignment as u64) as usize
}

/// The shared [`molt_arch::align_up`] on this platform's address width.
fn align_up(value: usize, alignment: usize) -> Option<usize> {
    molt_arch::align_up(value as u64, alignment as u64).map(|value| value as usize)
}

/// Assembles an Sv39 PTE from a physical address and permission flags.
fn pte(pa: u64, flags: u64) -> u64 {
    ((pa >> 12) << 10) | flags | PTE_V
}

/// Allocates one frame and returns its physical base address.
fn alloc_frame(frames: &mut FrameAllocator<'_>) -> Result<u64, PlatformError> {
    frames
        .allocate()
        .map(PhysicalFrame::start)
        .ok_or(PlatformError::Mapping(MappingError::OutOfFrames))
}

/// Allocates and zeroes one frame for use as a page table.
fn alloc_table(frames: &mut FrameAllocator<'_>) -> Result<*mut u64, PlatformError> {
    let frame = alloc_frame(frames)?;
    let table = frame as *mut u64;
    // SAFETY: every frame the allocator hands out is identity mapped — before
    // `init` because translation is off, after it because the writable span
    // covers all free RAM — and holds 512 aligned doublewords.
    unsafe {
        for index in 0..512 {
            table.add(index).write(0);
        }
    }
    Ok(table)
}

/// Enables Sv39 translation rooted at `root_phys` and flushes stale entries.
///
/// # Safety
///
/// `root_phys` must be a valid Sv39 root table whose mappings cover the
/// currently executing code and stack.
unsafe fn enable_sv39(root_phys: u64) {
    let satp = SATP_MODE_SV39 | (root_phys >> 12);
    // SAFETY: the flush brackets the `satp` write so no stale translation is used.
    unsafe {
        asm!(
            "csrw satp, {satp}",
            "sfence.vma",
            satp = in(reg) satp,
            options(nostack),
        );
    }
}
