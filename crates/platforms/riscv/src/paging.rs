//! Sv39 boot address space that maps the kernel image one section at a time.
//!
//! The earlier version identity-mapped all of RAM with a single RWX gigapage.
//! That kept the kernel running, but it also meant `.text` was writable and
//! `.data` was executable, so the W^X contract `MapPermissions` enforces on
//! x86_64 held nowhere on RISC-V. [`init`] instead walks the linker-exported
//! section bounds and gives each span exactly the rights it needs — `.text`
//! read/execute, `.rodata` read-only, everything writable from `.data` to the
//! end of RAM read/write — so a stray write to code or a jump into data faults.
//!
//! Two checks are built on top of it. [`verify_owned_mapping`] maps one private
//! page with [`MapPermissions`]-derived flags and round-trips a value through
//! the MMU. [`verify_image_protection`] reads the live page tables back and
//! confirms each section still holds the rights it was mapped with, which is
//! the check that would have failed against the old gigapage.

use core::arch::asm;
use core::cell::UnsafeCell;

use molt_arch::{
    BootInfo, FrameAllocator, FrameCursor, ImageSection, MapPermissions, MappingError,
    PageProtection, PhysicalFrame, PlatformError,
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
    static __molt_stack_top: u8;
    static __kernel_end: u8;
}

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
pub fn init(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let mut frames = FrameAllocator::new(boot_info.memory_map());
    let root = alloc_table(&mut frames)?;

    // Identity mappings throughout: `satp` is enabled from code that is running
    // at its physical address, so any virtual-to-physical shift would fault on
    // the instruction right after the `csrw`.
    //
    // The OpenSBI region below the payload address is deliberately left out: it
    // is M-mode firmware, and nothing in S-mode has any business reaching it.
    map_range(root, &mut frames, bound!(__text_start), bound!(__text_end), PTE_R | PTE_X)?;
    map_range(root, &mut frames, bound!(__rodata_start), bound!(__rodata_end), PTE_R)?;
    // `.data`, `.bss`, and the boot stack are one writable span, and the free
    // RAM after the image carries the same rights: page-table frames come out
    // of it, so it must stay reachable once translation is on.
    let writable_end = usize::try_from(ram_end(boot_info)).unwrap_or(bound!(__kernel_end));
    map_range(root, &mut frames, bound!(__data_start), writable_end, PTE_R | PTE_W)?;

    let cursor = frames.cursor();
    // SAFETY: every address the kernel executes from, reads, or writes — code,
    // constants, stack, and the page tables themselves — was just identity
    // mapped, so translation can be switched on in place.
    unsafe {
        enable_sv39(root as u64);
    }
    // SAFETY: same reasoning as `active`; this runs once on the boot hart.
    unsafe {
        *ACTIVE.0.get() = Some(BootPaging { root, cursor });
    }
    Ok(())
}

/// One past the last usable physical byte the firmware reported.
fn ram_end(boot_info: &BootInfo<'_>) -> u64 {
    let map = boot_info.memory_map();
    let mut end = 0;
    for index in 0..map.len() {
        if let Some(region) = map.region(index) {
            end = end.max(region.end());
        }
    }
    end
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
    unsafe {
        pointer.write_volatile(PROBE_VALUE);
        if pointer.read_volatile() != PROBE_VALUE {
            return Err(PlatformError::Mapping(MappingError::Backend));
        }
    }
    Ok(())
}

/// A constant that must live in `.rodata` for the image audit to have a target.
static RODATA_PROBE: u8 = 0x4d;

/// Reads the live tables back and checks each image section obeys W^X.
pub fn verify_image_protection() -> Result<(), PlatformError> {
    let state = active()?;
    let probes = [
        // A function address rather than `__text_start`: it proves the check
        // looks at a page the kernel is really executing from.
        (ImageSection::Text, verify_image_protection as *const () as usize),
        (ImageSection::Text, bound!(__text_start)),
        (ImageSection::Text, bound!(__text_end) - 1),
        (ImageSection::Rodata, (&raw const RODATA_PROBE) as usize),
        (ImageSection::Rodata, bound!(__rodata_start)),
        (ImageSection::Data, bound!(__data_start)),
        // The boot stack shares the writable span; a non-writable stack would
        // fault on the next call rather than at a place worth debugging.
        (ImageSection::Data, bound!(__molt_stack_top) - 1),
        (ImageSection::Data, bound!(__kernel_end)),
    ];
    for (section, address) in probes {
        let granted = protection(state.root, address)
            .ok_or(PlatformError::Mapping(MappingError::Unmapped))?;
        section.verify(granted).map_err(PlatformError::Mapping)?;
    }
    Ok(())
}

/// Walks the live tables and reports the rights `va` actually holds.
fn protection(root: *const u64, va: usize) -> Option<PageProtection> {
    let mut table = root;
    for level in (0..=2).rev() {
        // SAFETY: `table` points at a 512-entry table frame, identity mapped
        // read/write by `init`, and the index is masked to nine bits.
        let entry = unsafe { *table.add(index(va, level)) };
        if entry & PTE_V == 0 {
            return None;
        }
        if entry & PTE_RWX != 0 {
            return Some(PageProtection::new(
                entry & PTE_R != 0,
                entry & PTE_W != 0,
                entry & PTE_X != 0,
            ));
        }
        table = ((entry >> 10) << 12) as *const u64;
    }
    None
}

fn map_range(
    root: *mut u64,
    frames: &mut FrameAllocator<'_>,
    start: usize,
    end: usize,
    rights: u64,
) -> Result<(), PlatformError> {
    let mut flags = rights | PTE_A;
    if rights & PTE_W != 0 {
        flags |= PTE_D;
    }
    let mut va = align_down(start, PAGE_4K);
    let end = align_up(end, PAGE_4K).ok_or(PlatformError::Mapping(MappingError::InvalidAddress))?;
    while va < end {
        let level = if va % PAGE_2M == 0 && end - va >= PAGE_2M { 1 } else { 0 };
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

fn align_down(value: usize, alignment: usize) -> usize {
    value & !(alignment - 1)
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value.checked_add(alignment - 1).map(|value| value & !(alignment - 1))
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
