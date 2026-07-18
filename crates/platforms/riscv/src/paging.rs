//! Minimal Sv39 paging groundwork with a W^X-checked owned-mapping probe.
//!
//! The probe mirrors the x86_64 `verify_owned_mapping` check: it allocates
//! physical frames for a fresh page table, identity-maps all of RAM with a
//! single gigapage so the running kernel keeps executing, then maps one private
//! 4 KiB page with writable-but-not-executable permissions, enables Sv39, and
//! writes and reads that page back through the MMU. The permission bits come
//! from [`MapPermissions`], which rejects a writable *and* executable mapping at
//! construction, so W^X is enforced before a leaf entry is ever formed.

use core::arch::asm;

use molt_arch::{
    BootInfo, FrameAllocator, MapPermissions, MappingError, PhysicalFrame, PlatformError,
};

use crate::RAM_BASE;

/// Sv39 page-table entry flags.
const PTE_V: u64 = 1 << 0; // valid
const PTE_R: u64 = 1 << 1; // readable
const PTE_W: u64 = 1 << 2; // writable
const PTE_X: u64 = 1 << 3; // executable
const PTE_A: u64 = 1 << 6; // accessed
const PTE_D: u64 = 1 << 7; // dirty

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

    // Identity-map the 1 GiB region containing RAM as a single RWX gigapage so
    // that code, stack, and freshly allocated page-table frames stay reachable
    // once translation is enabled.
    let gigapage = RAM_BASE & !((1 << 30) - 1);
    let vpn2 = (gigapage >> 30) & 0x1ff;
    // SAFETY: `root` is a freshly allocated, zeroed, 512-entry table frame.
    unsafe {
        root.add(vpn2).write(pte(gigapage as u64, PTE_R | PTE_W | PTE_X));
    }

    // Derive the probe's leaf flags from W^X-checked permissions. A writable
    // RISC-V leaf must also be readable, and must never be executable here.
    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let mut leaf = PTE_V | PTE_A | PTE_D;
    if permissions.is_writable() {
        leaf |= PTE_R | PTE_W;
    }
    if permissions.is_executable() {
        leaf |= PTE_X;
    }

    let probe = alloc_frame(&mut frames)?;
    map_4k(root, &mut frames, PROBE_VA, probe, leaf & !PTE_V)?;

    // SAFETY: the identity gigapage covers every address the kernel touches,
    // including this table, so translation can be enabled in place.
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
