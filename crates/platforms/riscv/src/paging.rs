//! Sv39 identity mappings for the W^X kernel image and an owned-page probe.
//!
//! The probe mirrors the x86_64 `verify_owned_mapping` check: it allocates
//! physical frames for a fresh page table, identity-maps each kernel section
//! with its linker-declared permissions, then maps one private 4 KiB page with
//! writable-but-not-executable permissions. Before enabling Sv39 it walks every
//! kernel PTE to prove that no large or writable-executable leaf slipped in.

use core::arch::asm;

use molt_arch::{
    BootInfo, FRAME_SIZE, FrameAllocator, MapPermissions, MappingError, PhysicalFrame,
    PlatformError,
};

/// Sv39 page-table entry flags.
const PTE_V: u64 = 1 << 0; // valid
const PTE_R: u64 = 1 << 1; // readable
const PTE_W: u64 = 1 << 2; // writable
const PTE_X: u64 = 1 << 3; // executable
const PTE_A: u64 = 1 << 6; // accessed
const PTE_D: u64 = 1 << 7; // dirty
/// Permission bits carried by a leaf entry.
const PTE_PERMISSIONS: u64 = PTE_R | PTE_W | PTE_X;

/// `satp.MODE` value selecting three-level Sv39 translation.
const SATP_MODE_SV39: u64 = 8 << 60;

/// A private virtual address, outside the identity gigapage, for the probe page.
const PROBE_VA: usize = 0x2000_0000;
/// Distinctive payload written through the probe mapping ("MOLT_WX").
const PROBE_VALUE: u64 = 0x004d_4f4c_545f_5758;

/// Builds an Sv39 mapping, enables it, and verifies an owned W^X page.
pub fn verify_owned_mapping(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let mut frames = FrameAllocator::new(boot_info.memory_map());
    let root = alloc_table(&mut frames)?;

    for section in kernel_sections() {
        let permissions = MapPermissions::new(section.writable, section.executable)
            .map_err(PlatformError::Mapping)?;
        map_identity_range(root, &mut frames, section.start, section.end, leaf_flags(permissions))?;
    }
    verify_kernel_permissions(root)?;

    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let probe = alloc_frame(&mut frames)?;
    map_4k(root, &mut frames, PROBE_VA, probe, leaf_flags(permissions))?;

    // SAFETY: the section mappings cover all executing code, statics, and the
    // current boot stack, so translation can be enabled in place.
    unsafe {
        enable_sv39(root as u64);
    }

    // Exercise the mapping through the MMU.
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

#[derive(Clone, Copy)]
struct KernelSection {
    start: usize,
    end: usize,
    writable: bool,
    executable: bool,
}

/// Returns the page-aligned bounds exported by the kernel linker script.
fn kernel_sections() -> [KernelSection; 3] {
    unsafe extern "C" {
        static __text_start: u8;
        static __text_end: u8;
        static __rodata_start: u8;
        static __rodata_end: u8;
        static __data_start: u8;
        static __kernel_end: u8;
    }

    [
        KernelSection {
            start: (&raw const __text_start) as usize,
            end: (&raw const __text_end) as usize,
            writable: false,
            executable: true,
        },
        KernelSection {
            start: (&raw const __rodata_start) as usize,
            end: (&raw const __rodata_end) as usize,
            writable: false,
            executable: false,
        },
        KernelSection {
            start: (&raw const __data_start) as usize,
            end: (&raw const __kernel_end) as usize,
            writable: true,
            executable: false,
        },
    ]
}

/// Forms a readable RISC-V leaf from W^X-checked portable permissions.
fn leaf_flags(permissions: MapPermissions) -> u64 {
    let mut flags = PTE_R | PTE_A;
    if permissions.is_writable() {
        flags |= PTE_W | PTE_D;
    }
    if permissions.is_executable() {
        flags |= PTE_X;
    }
    flags
}

fn map_identity_range(
    root: *mut u64,
    frames: &mut FrameAllocator<'_>,
    start: usize,
    end: usize,
    flags: u64,
) -> Result<(), PlatformError> {
    if start >= end || start % FRAME_SIZE as usize != 0 || end % FRAME_SIZE as usize != 0 {
        return Err(PlatformError::Mapping(MappingError::InvalidAddress));
    }
    let mut address = start;
    while address < end {
        map_4k(root, frames, address, address as u64, flags)?;
        address += FRAME_SIZE as usize;
    }
    Ok(())
}

/// Confirms that every kernel page is a 4 KiB leaf with its section's permissions.
fn verify_kernel_permissions(root: *mut u64) -> Result<(), PlatformError> {
    for section in kernel_sections() {
        let permissions = MapPermissions::new(section.writable, section.executable)
            .map_err(PlatformError::Mapping)?;
        let mut address = section.start;
        while address < section.end {
            if leaf_permissions(root, address) != Some(leaf_flags(permissions) & PTE_PERMISSIONS) {
                return Err(PlatformError::Mapping(MappingError::Backend));
            }
            address += FRAME_SIZE as usize;
        }
    }
    Ok(())
}

/// Returns the R/W/X bits when `va` resolves through a 4 KiB leaf.
fn leaf_permissions(root: *mut u64, va: usize) -> Option<u64> {
    let index = |level: usize| (va >> (12 + 9 * level)) & 0x1ff;
    let mut table = root;
    for level in (0..=2).rev() {
        // SAFETY: callers pass a valid root and every pointer entry names a table frame.
        let entry = unsafe { table.add(index(level)).read() };
        if entry & PTE_V == 0 {
            return None;
        }
        if entry & PTE_PERMISSIONS != 0 {
            return (level == 0).then_some(entry & PTE_PERMISSIONS);
        }
        table = (((entry >> 10) << 12) as usize) as *mut u64;
    }
    None
}

/// Maps a single 4 KiB page, allocating intermediate tables as needed.
///
/// `leaf_flags` carries the permission bits without `PTE_V`, which this adds.
fn map_4k(
    root: *mut u64,
    frames: &mut FrameAllocator<'_>,
    va: usize,
    pa: u64,
    leaf_flags: u64,
) -> Result<(), PlatformError> {
    let index = |level: usize| (va >> (12 + 9 * level)) & 0x1ff;
    let mut table = root;
    for level in (1..=2).rev() {
        // SAFETY: `table` always points at a valid 512-entry table frame.
        let entry = unsafe { &mut *table.add(index(level)) };
        if *entry & PTE_V == 0 {
            let next = alloc_table(frames)?;
            *entry = pte(next as u64, 0); // a pointer entry clears R/W/X.
        }
        let next_phys = (*entry >> 10) << 12;
        table = next_phys as *mut u64;
    }
    // SAFETY: `table` is the level-0 table for `va`.
    unsafe {
        table.add(index(0)).write(pte(pa, leaf_flags));
    }
    Ok(())
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
    // SAFETY: paging is still disabled, so this physical frame is directly
    // addressable; it is 4 KiB aligned and holds 512 doublewords.
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
