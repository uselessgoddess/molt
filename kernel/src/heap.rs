//! The kernel heap, donated once at boot.
//!
//! RAM claimed from the platform and handed to [`Global`]. It is never given
//! back: the frames stay the heap's for the life of the kernel, and the free
//! list inside recycles them.

use molt_alloc::{Global, Interrupt};
use molt_arch::{BootInfo, Platform, PlatformError};

/// Frames per claim, and how many claims. A heap needs no contiguous span, so
/// it asks in chunks and takes what a usable region can still give: the
/// platform hands them out in rising order, and the free list merges the ones
/// that turn out adjacent. Four megabytes covers the filesystem's tree nodes,
/// arena bitmap, and block scratch, and leaves the rest for DMA.
const CHUNK: u64 = 64;
const CHUNKS: u64 = 16;

#[global_allocator]
static HEAP: Global = Global::new();

/// Donates boot RAM to the global allocator and reports the bytes it took.
pub fn init<P: Platform>(
    boot_info: &BootInfo<'_>,
    platform: &mut P,
) -> Result<usize, PlatformError> {
    let offset = boot_info.physical_offset().ok_or(PlatformError::MissingPhysicalMemoryMap)?;
    for _ in 0..CHUNKS {
        // A chunk that crosses a gap in the map ends the donation rather than
        // failing the boot; what the heap already holds is still a heap.
        let Ok(span) = platform.claim_ram(boot_info, CHUNK) else {
            break;
        };
        let start = (offset + span.start()) as *mut u8;
        // SAFETY: the platform hands each span out once and keeps no claim on
        // it, usable RAM is mapped read-write at `offset`, and the heap
        // outlives the kernel it serves.
        unsafe { HEAP.extend(start, span.bytes() as usize) };
    }
    match HEAP.size() {
        0 => Err(PlatformError::Unsupported),
        bytes => Ok(bytes),
    }
}

/// Bytes held by live allocations.
pub fn used() -> usize {
    HEAP.used()
}

/// Bars the heap for the interrupt the caller is about to service.
///
/// Held for the whole handler: the heap's lock is a spin flag, so an allocation
/// from an interrupt that arrived while the flag was held would spin forever.
pub fn interrupt() -> Interrupt<'static> {
    HEAP.interrupt()
}
