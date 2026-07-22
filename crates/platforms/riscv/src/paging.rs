//! Kernel-owned Sv39 translation tables.
//!
//! [`init`] maps each image section with W^X rights and only firmware-usable RAM
//! beyond the image. Reserved ranges and device holes remain unmapped so the
//! live-table audit can reject stray leaves.

use core::arch::asm;
use core::cell::UnsafeCell;

use molt_arch::audit::{Audit, Declared, Leaf, MappedRange, PageWalk};
use molt_arch::memory::{Cache, Inventory, Kind, Rights, Span};
use molt_arch::{
    BootInfo, FrameAllocator, FrameCursor, ImageSection, MapPermissions, MappingError,
    PageProtection, PhysicalFrame, PlatformError, UsableRegions,
};

const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

/// Non-zero permission bits distinguish a leaf from a table pointer.
const PTE_RWX: u64 = PTE_R | PTE_W | PTE_X;

const SATP_MODE_SV39: u64 = 8 << 60;

const PAGE_4K: usize = 4096;
const PAGE_2M: usize = 2 * 1024 * 1024;

const PROBE_VA: usize = 0x2000_0000;
const PROBE_VALUE: u64 = 0x004d_4f4c_545f_5758;

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

const MAX_RAM_RANGES: usize = 8;

type MappingLog = Declared<{ 3 + MAX_RAM_RANGES + 1 }>;

macro_rules! bound {
    ($symbol:ident) => {
        (&raw const $symbol) as usize
    };
}

struct BootPaging {
    root: *mut u64,
    cursor: FrameCursor,
    log: MappingLog,
}

struct Active(UnsafeCell<Option<BootPaging>>);

// SAFETY: the boot address space is built and used on the single boot hart
// before any other hart is started, so there is no concurrent access to share.
unsafe impl Sync for Active {}

static ACTIVE: Active = Active(UnsafeCell::new(None));

fn active() -> Result<&'static mut BootPaging, PlatformError> {
    // SAFETY: single boot hart, traps do not touch this cell, and the returned
    // borrow is confined to one call.
    unsafe { &mut *ACTIVE.0.get() }.as_mut().ok_or(PlatformError::Mapping(MappingError::Unmapped))
}

/// Builds the per-section boot address space and enables Sv39.
pub fn init(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let kernel_end = bound!(__kernel_end) as u64;
    let mut frames = FrameAllocator::above(boot_info.memory_map(), kernel_end);
    let root = alloc_table(&mut frames)?;

    let mut log = MappingLog::new();
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
        *ACTIVE.0.get() = Some(BootPaging { root, cursor, log });
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
    let aligned_start = align_down(start, PAGE_4K) as u64;
    let aligned_end =
        align_up(end, PAGE_4K).ok_or(PlatformError::Mapping(MappingError::InvalidAddress))? as u64;
    log.push(MappedRange::section(section, aligned_start, aligned_end))
        .map_err(PlatformError::Mapping)
}

pub fn verify_owned_mapping(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);

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
    // Remove the probe before auditing the declared mappings.
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
    walk_leaves(state.root, &state.log.audit(), &inventory)
}

const UART_MMIO: u64 = 0x1000_0000;
const UART_WINDOW: usize = 0x3000_0000;
const UART_THR: usize = 0;
const UART_LSR: usize = 5;
const UART_LSR_THRE: u8 = 1 << 5;

/// Maps, exercises, and audits a typed UART device window.
pub fn verify_device_window(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let inventory = Inventory::new(boot_info.memory_map());
    let span = Span::frames(UART_MMIO, 1).map_err(|_| address_error())?;
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

    let walk = TableWalk { root: state.root, inventory: &inventory };
    state.log.audit().cover(&walk).map_err(PlatformError::Mapping)?;
    walk_leaves(state.root, &state.log.audit(), &inventory)
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
        if level == 0 {
            return Err(PlatformError::Mapping(MappingError::Backend));
        }
        let next = ((entry >> 10) << 12) as *const u64;
        walk_table(next, level - 1, start, audit, inventory)?;
    }
    Ok(())
}

/// Decodes leaf rights and the physical memory attribute.
///
/// This target lacks `Svpbmt`, so the firmware map supplies the PMA: described
/// memory is write-back and holes are device-ordered.
fn protection(entry: u64, inventory: &Inventory<'_>) -> PageProtection {
    let physical = (entry >> 10) << 12;
    let cache = match inventory.kind(physical) {
        Kind::Ram | Kind::Image | Kind::Reserved => Cache::WriteBack,
        Kind::Device => Cache::Device,
    };
    PageProtection::new(entry & PTE_R != 0, entry & PTE_W != 0, entry & PTE_X != 0).cached(cache)
}

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
            // SAFETY: `table` is a readable 512-entry root or identity-mapped child,
            // and the index is masked to nine bits.
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
        // SAFETY: `table` points to a valid 512-entry table frame.
        let entry = unsafe { &mut *table.add(index(va, above)) };
        if *entry & PTE_V == 0 {
            let next = alloc_table(frames)?;
            *entry = pte(next as u64, 0);
        } else if *entry & PTE_RWX != 0 {
            // Ranges are mapped once; splitting a leaf is unsupported.
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

fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * level)) & 0x1ff
}

fn align_down(value: usize, alignment: usize) -> usize {
    molt_arch::align_down(value as u64, alignment as u64) as usize
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    molt_arch::align_up(value as u64, alignment as u64).map(|value| value as usize)
}

fn pte(pa: u64, flags: u64) -> u64 {
    ((pa >> 12) << 10) | flags | PTE_V
}

fn alloc_frame(frames: &mut FrameAllocator<'_>) -> Result<u64, PlatformError> {
    frames
        .allocate()
        .map(PhysicalFrame::start)
        .ok_or(PlatformError::Mapping(MappingError::OutOfFrames))
}

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
