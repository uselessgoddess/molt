//! Registered DMA memory: frames a device reads and writes, addressed both ways
//! at once.
//!
//! A driver hands the device physical addresses and touches the same bytes
//! through a CPU pointer. [`Region`] carries the pair, so a public operation
//! never passes a raw physical address around, and [`Arena`] hands regions out
//! of one span of [`Owner::Device`] frames it
//! reclaims as a whole once the device has been told to stop.

use crate::memory::{Error as MemoryError, FrameTable, Frames, Owner, Span};
use crate::{FRAME_SIZE, FrameAllocator};

/// Why a DMA request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaError {
    /// The access leaves the region.
    Range,
    /// The offset is not a multiple of the access width.
    Alignment,
    /// A request for zero bytes, which names no frame.
    Empty,
    /// The frames the allocator returned are not one contiguous span.
    NotContiguous,
    /// The allocator ran out before the arena's span was filled.
    OutOfFrames,
    /// The arena has no room left for a region of this size.
    OutOfSpace,
    /// The frame table refused the claim or release.
    Frames(MemoryError),
}

impl From<MemoryError> for DmaError {
    fn from(error: MemoryError) -> Self {
        Self::Frames(error)
    }
}

/// A registered DMA window: the same bytes reached as a CPU pointer and as a
/// physical address.
///
/// The device reads and writes through [`physical`](Region::physical); the
/// driver touches the same bytes through the checked accessors, which the CPU
/// sees because the region is plain write-back RAM. Like [`Mmio`](crate::Mmio)
/// it is `Send` but not `Sync`: a DMA buffer is order-sensitive, so sharing one
/// across cores is a decision a driver makes explicitly.
#[derive(Debug)]
pub struct Region {
    cpu: *mut u8,
    physical: u64,
    len: u64,
}

// SAFETY: `Region` is a unique handle to a range of frames
unsafe impl Send for Region {}

impl Region {
    /// Wraps `len` bytes reachable at `cpu` and physically at `physical`.
    ///
    /// # Safety
    ///
    /// `cpu` must be the live, write-back direct-map address of the `len` bytes
    /// at `physical`; those frames must be uniquely owned for the region's
    /// lifetime, and no second region may cover any part of the same range.
    pub const unsafe fn new(cpu: *mut u8, physical: u64, len: u64) -> Self {
        Self { cpu, physical, len }
    }

    /// The physical address a device is given to reach these bytes.
    pub const fn physical(&self) -> u64 {
        self.physical
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn read_u8(&self, offset: u64) -> Result<u8, DmaError> {
        let address = self.access(offset, 1)?;
        // SAFETY: `access` checked bounds and alignment, and the constructor
        // guarantees the region is mapped for the handle's lifetime.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn read_u16(&self, offset: u64) -> Result<u16, DmaError> {
        let address = self.access(offset, 2)?.cast::<u16>();
        // SAFETY: see `read_u8`; `access` also proved the offset is aligned.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn read_u32(&self, offset: u64) -> Result<u32, DmaError> {
        let address = self.access(offset, 4)?.cast::<u32>();
        // SAFETY: see `read_u8`; `access` also proved the offset is aligned.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn read_u64(&self, offset: u64) -> Result<u64, DmaError> {
        let address = self.access(offset, 8)?.cast::<u64>();
        // SAFETY: see `read_u8`; `access` also proved the offset is aligned.
        Ok(unsafe { address.read_volatile() })
    }

    pub fn write_u8(&self, offset: u64, value: u8) -> Result<(), DmaError> {
        let address = self.access(offset, 1)?;
        // SAFETY: see `read_u8`. The region is not `Sync`, so no other thread
        // holds a reference through which to race this write.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    pub fn write_u16(&self, offset: u64, value: u16) -> Result<(), DmaError> {
        let address = self.access(offset, 2)?.cast::<u16>();
        // SAFETY: see `write_u8`.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    pub fn write_u32(&self, offset: u64, value: u32) -> Result<(), DmaError> {
        let address = self.access(offset, 4)?.cast::<u32>();
        // SAFETY: see `write_u8`.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    pub fn write_u64(&self, offset: u64, value: u64) -> Result<(), DmaError> {
        let address = self.access(offset, 8)?.cast::<u64>();
        // SAFETY: see `write_u8`.
        unsafe { address.write_volatile(value) };
        Ok(())
    }

    /// Copies `bytes` into the region at `offset`.
    ///
    /// Ordering against the device is the caller's: a fence publishes the write
    /// before the descriptor that points a device at it, so the copy itself is
    /// not volatile.
    pub fn write_from(&self, offset: u64, bytes: &[u8]) -> Result<(), DmaError> {
        let address = self.span(offset, bytes.len() as u64)?;
        // SAFETY: `span` proved `offset + len` fits, and `bytes` cannot alias
        // the uniquely owned region it is copied into.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), address, bytes.len()) };
        Ok(())
    }

    /// Copies `bytes.len()` bytes out of the region at `offset`.
    pub fn read_into(&self, offset: u64, bytes: &mut [u8]) -> Result<(), DmaError> {
        let address = self.span(offset, bytes.len() as u64)?;
        // SAFETY: `span` proved `offset + len` fits, and `bytes` cannot alias
        // the uniquely owned region it is copied from.
        unsafe { core::ptr::copy_nonoverlapping(address, bytes.as_mut_ptr(), bytes.len()) };
        Ok(())
    }

    /// Clears the whole region to zero.
    pub fn zero(&self) {
        // SAFETY: the constructor established `len` bytes at `cpu` as uniquely
        // owned and mapped for the region's lifetime.
        unsafe { core::ptr::write_bytes(self.cpu, 0, self.len as usize) };
    }

    /// Checks one access of `width` bytes and returns the address it names.
    fn access(&self, offset: u64, width: u64) -> Result<*mut u8, DmaError> {
        if offset % width != 0 {
            return Err(DmaError::Alignment);
        }
        self.span(offset, width)
    }

    /// Rejects a range that does not fit entirely inside the region.
    fn span(&self, offset: u64, len: u64) -> Result<*mut u8, DmaError> {
        match offset.checked_add(len) {
            Some(end) if end <= self.len => {
                // SAFETY: the offset was just proved to lie within the region.
                Ok(unsafe { self.cpu.add(offset as usize) })
            }
            _ => Err(DmaError::Range),
        }
    }
}

/// One span of device-owned frames, handed out as [`Region`]s and reclaimed
/// whole.
///
/// The arena claims a bounded, contiguous span as a single
/// [`Owner::Device`] [`Frames`] token, then
/// bump-allocates frame-granular regions out of it. It is reclaimed by
/// [`reset`](Arena::reset), which the caller invokes only after the device has
/// been told to stop touching the frames.
pub struct Arena<'s> {
    table: FrameTable<'s>,
    frames: Frames,
    offset: u64,
    next: u64,
}

impl<'s> Arena<'s> {
    /// Claims `slots.len()` contiguous frames from `allocator` as one device
    /// span, tagged `tag` and addressed for the CPU through `offset`.
    ///
    /// The frames must come out contiguous — the allocator hands out a rising
    /// sequence within one usable region, so this holds until a request spans a
    /// gap in the map, which is refused rather than papered over.
    pub fn claim(
        allocator: &mut FrameAllocator<'_>,
        offset: u64,
        tag: u32,
        slots: &'s mut [Option<Owner>],
    ) -> Result<Self, DmaError> {
        let count = slots.len() as u64;
        if count == 0 {
            return Err(DmaError::Empty);
        }
        let first = allocator.allocate().ok_or(DmaError::OutOfFrames)?.start();
        let mut previous = first;
        for _ in 1..count {
            let frame = allocator.allocate().ok_or(DmaError::OutOfFrames)?.start();
            if frame != previous + FRAME_SIZE {
                return Err(DmaError::NotContiguous);
            }
            previous = frame;
        }

        let span = Span::frames(first, count)?;
        let mut table = FrameTable::over(span, slots)?;
        let frames = table.claim(span, Owner::Device(tag))?;
        Ok(Self { table, frames, offset, next: first })
    }

    /// The device span this arena owns.
    pub fn span(&self) -> Span {
        self.frames.span()
    }

    /// Bump-allocates a region of `bytes`, backed by whole frames.
    ///
    /// The region reports the exact `bytes` asked for, so its accessors stay
    /// tightly bounded, while the frames behind it belong to no other region.
    pub fn region(&mut self, bytes: u64) -> Result<Region, DmaError> {
        if bytes == 0 {
            return Err(DmaError::Empty);
        }
        let step = crate::align_up(bytes, FRAME_SIZE).ok_or(DmaError::OutOfSpace)?;
        let physical = self.next;
        let end = physical.checked_add(step).ok_or(DmaError::OutOfSpace)?;
        if end > self.frames.span().end() {
            return Err(DmaError::OutOfSpace);
        }
        self.next = end;

        let cpu = (self.offset + physical) as *mut u8;
        // SAFETY: `physical` names whole frames inside the span this arena
        // claimed as `Owner::Device`, `offset` is their live write-back direct
        // map, the bump cursor never re-issues a frame, and `bytes <= step`.
        Ok(unsafe { Region::new(cpu, physical, bytes) })
    }

    /// Releases the whole span back to its frame table.
    ///
    /// The caller must already have told the device to stop, so no in-flight
    /// DMA can land in a frame after it is reclaimed.
    pub fn reset(self) -> Result<(), DmaError> {
        let Self { mut table, frames, .. } = self;
        table.release(frames)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Arena, DmaError, Region};
    use crate::{FRAME_SIZE, FrameAllocator, MemoryMap, MemoryRegion, MemoryRegionKind};

    struct Map<'r>(&'r [MemoryRegion]);

    impl MemoryMap for Map<'_> {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn region(&self, index: usize) -> Option<MemoryRegion> {
            self.0.get(index).copied()
        }
    }

    fn usable(start: u64, end: u64) -> MemoryRegion {
        MemoryRegion::new(start, end, MemoryRegionKind::Usable)
    }

    fn region(bytes: &mut [u8], physical: u64) -> Region {
        // SAFETY: the slice is live for the borrow, uniquely borrowed.
        unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) }
    }

    #[test]
    fn region_addresses_same_bytes_two_ways() {
        let mut buffer = [0u8; 16];
        let region = region(&mut buffer, 0xdead_0000);

        region.write_u32(4, 0x0102_0304).expect("aligned write inside the region");

        assert_eq!(region.physical(), 0xdead_0000);
        assert_eq!(u32::from_le_bytes(buffer[4..8].try_into().unwrap()), 0x0102_0304);
    }

    #[test]
    fn bulk_copy_round_trips() {
        let mut buffer = [0u8; 8];
        let region = region(&mut buffer, 0x1000);

        region.write_from(0, &[1, 2, 3, 4]).expect("a copy inside the region");
        let mut read = [0u8; 4];
        region.read_into(0, &mut read).expect("a copy inside the region");

        assert_eq!(read, [1, 2, 3, 4]);
    }

    #[test]
    fn access_past_region_refused() {
        let mut buffer = [0u8; 16];
        let region = region(&mut buffer, 0x2000);

        assert_eq!(region.read_u32(16), Err(DmaError::Range));
        assert_eq!(region.write_from(13, &[0, 0, 0, 0]), Err(DmaError::Range));
    }

    #[test]
    fn misaligned_access_refused() {
        let mut buffer = [0u8; 16];
        let region = region(&mut buffer, 0x3000);

        assert_eq!(region.read_u32(2), Err(DmaError::Alignment));
        assert_eq!(region.write_u16(1, 0), Err(DmaError::Alignment));
    }

    #[test]
    fn arena_disjoint_frame_regions() {
        let regions = [usable(0x10_0000, 0x10_0000 + 8 * FRAME_SIZE)];
        let map = Map(&regions);
        let mut allocator = FrameAllocator::new(&map);
        let mut slots = [None; 8];
        let mut arena = Arena::claim(&mut allocator, 0, 7, &mut slots).expect("eight free frames");

        let first = arena.region(16).expect("room for a header");
        let second = arena.region(FRAME_SIZE + 1).expect("room for two more frames");

        assert_eq!(first.physical(), 0x10_0000);
        assert_eq!(second.physical(), 0x10_0000 + FRAME_SIZE, "regions shared a frame");
        assert_eq!(arena.span().count(), 8);
    }

    #[test]
    fn arena_refuses_region_past_span() {
        let regions = [usable(0x10_0000, 0x10_0000 + 2 * FRAME_SIZE)];
        let map = Map(&regions);
        let mut allocator = FrameAllocator::new(&map);
        let mut slots = [None; 2];
        let mut arena = Arena::claim(&mut allocator, 0, 1, &mut slots).expect("two free frames");

        assert_eq!(arena.region(3 * FRAME_SIZE).err(), Some(DmaError::OutOfSpace));
    }

    #[test]
    fn span_gap_refused() {
        let regions = [
            usable(0x10_0000, 0x10_0000 + FRAME_SIZE),
            usable(0x10_0000 + 2 * FRAME_SIZE, 0x10_0000 + 3 * FRAME_SIZE),
        ];
        let map = Map(&regions);
        let mut allocator = FrameAllocator::new(&map);
        let mut slots = [None; 2];

        assert_eq!(
            Arena::claim(&mut allocator, 0, 0, &mut slots).err(),
            Some(DmaError::NotContiguous)
        );
    }

    #[test]
    fn reset_returns_span_to_table() {
        let regions = [usable(0x10_0000, 0x10_0000 + 4 * FRAME_SIZE)];
        let map = Map(&regions);
        let mut allocator = FrameAllocator::new(&map);
        let mut slots = [None; 4];
        let arena = Arena::claim(&mut allocator, 0, 2, &mut slots).expect("four free frames");

        arena.reset().expect("the span this arena claimed");

        assert!(slots.iter().all(Option::is_none), "reset left frames claimed");
    }
}
