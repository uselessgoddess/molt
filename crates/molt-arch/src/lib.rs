#![no_std]

//! Hardware-independent contracts shared by the kernel and architecture crates.

pub mod asid;
pub mod audit;
pub mod cpu;
pub mod dma;
pub mod iommu;
pub mod irq;
pub mod memory;
pub mod mmio;
pub mod pci;
pub mod refcount;
pub mod shootdown;
pub mod smp;
pub mod va;
pub mod view;

use core::fmt;

pub use molt_core::cpu::CpuId;

pub use crate::cpu::Local;
pub use crate::irq::{FabricError, InterruptFabric, MsiMessage, Sink};
pub use crate::memory::Cache;
pub use crate::mmio::{DeviceMapper, Mmio, MmioError};
pub use crate::pci::ConfigSpace;
pub use crate::shootdown::Tlb;
pub use crate::smp::{Entry, Smp, SmpError, Stack, number};
pub use crate::view::View;

/// Architecture-neutral information passed from a platform boot adapter.
#[derive(Clone, Copy)]
pub struct BootInfo<'boot> {
    memory_map: &'boot dyn MemoryMap,
    physical_offset: Option<u64>,
    kernel_image: Option<ImageRange>,
}

impl<'boot> BootInfo<'boot> {
    pub const fn new(memory_map: &'boot dyn MemoryMap, physical_offset: Option<u64>) -> Self {
        Self { memory_map, physical_offset, kernel_image: None }
    }

    /// Attaches the virtual range the loader placed the kernel image at.
    pub const fn with_kernel_image(mut self, image: ImageRange) -> Self {
        self.kernel_image = Some(image);
        self
    }

    pub const fn memory_map(&self) -> &'boot dyn MemoryMap {
        self.memory_map
    }

    pub const fn physical_offset(&self) -> Option<u64> {
        self.physical_offset
    }

    /// The kernel image's live virtual range, when the loader reports it.
    pub const fn kernel_image(&self) -> Option<ImageRange> {
        self.kernel_image
    }
}

/// Where a loader placed the kernel image once translation was set up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageRange {
    start: u64,
    len: u64,
}

impl ImageRange {
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn len(self) -> u64 {
        self.len
    }

    pub const fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Read-only physical memory map supplied by a platform boot adapter.
pub trait MemoryMap {
    fn len(&self) -> usize;

    fn region(&self, index: usize) -> Option<MemoryRegion>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One half-open physical address range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRegion {
    start: u64,
    end: u64,
    kind: MemoryRegionKind,
}

impl MemoryRegion {
    pub const fn new(start: u64, end: u64, kind: MemoryRegionKind) -> Self {
        Self { start, end, kind }
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn end(self) -> u64 {
        self.end
    }

    pub const fn kind(self) -> MemoryRegionKind {
        self.kind
    }

    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Portable classification of firmware-provided physical memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRegionKind {
    Usable,
    Reserved,
    Bootloader,
    Firmware(u32),
}

pub const FRAME_SIZE: u64 = 4096;

/// One aligned 4 KiB physical frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalFrame(u64);

impl PhysicalFrame {
    pub const fn start(self) -> u64 {
        self.0
    }
}

/// Rounds `value` down to a multiple of `alignment`, which must be a power of two.
pub const fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

/// Rounds `value` up to a multiple of `alignment`, or `None` when that overflows.
///
/// `alignment` must be a power of two.
pub const fn align_up(value: u64, align: u64) -> Option<u64> {
    match value.checked_add(align - 1) {
        Some(value) => Some(align_down(value, align)),
        None => None,
    }
}

/// The complete aligned frames inside a firmware-usable region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsableRange {
    start: u64,
    end: u64,
}

impl UsableRange {
    /// The frames of `region` that lie at or above `floor`, or `None` when the
    /// region is not usable RAM, or holds no whole frame above the floor.
    pub fn of(region: MemoryRegion, floor: u64) -> Option<Self> {
        if region.kind() != MemoryRegionKind::Usable {
            return None;
        }
        let start = align_up(region.start().max(floor), FRAME_SIZE)?;
        let end = align_down(region.end(), FRAME_SIZE);
        if start + FRAME_SIZE <= end { Some(Self { start, end }) } else { None }
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn end(self) -> u64 {
        self.end
    }

    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Aligned usable RAM above a floor, shared by allocation and direct mapping.
pub struct UsableRegions<'m> {
    map: &'m dyn MemoryMap,
    region: usize,
    floor: u64,
}

impl<'m> UsableRegions<'m> {
    pub const fn above(map: &'m dyn MemoryMap, floor: u64) -> Self {
        Self { map, region: 0, floor }
    }
}

impl Iterator for UsableRegions<'_> {
    type Item = UsableRange;

    fn next(&mut self) -> Option<UsableRange> {
        while self.region < self.map.len() {
            let region = self.map.region(self.region);
            self.region += 1;
            if let Some(range) = region.and_then(|region| UsableRange::of(region, self.floor)) {
                return Some(range);
            }
        }
        None
    }
}

/// Allocation-free bump allocator over the usable ranges of a memory map.
pub struct FrameAllocator<'m> {
    map: &'m dyn MemoryMap,
    floor: u64,
    region: usize,
    next: u64,
}

/// Why a run of frames was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunError {
    /// A request for no frames names no span.
    Empty,
    /// The map ran out before the run was filled.
    OutOfFrames,
    /// The next frame sits past a gap in the map, so the run is not one span.
    NotContiguous,
}

/// A resumable [`FrameAllocator`] position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameCursor {
    floor: u64,
    region: usize,
    next: u64,
}

impl<'m> FrameAllocator<'m> {
    pub const fn new(map: &'m dyn MemoryMap) -> Self {
        Self::above(map, 0)
    }

    /// Hands out usable frames at or above `floor`.
    pub const fn above(map: &'m dyn MemoryMap, floor: u64) -> Self {
        Self { map, floor, region: 0, next: 0 }
    }

    /// Resumes allocation over `map` from a cursor an earlier allocator left.
    pub const fn resume(map: &'m dyn MemoryMap, cursor: FrameCursor) -> Self {
        Self { map, floor: cursor.floor, region: cursor.region, next: cursor.next }
    }

    pub const fn cursor(&self) -> FrameCursor {
        FrameCursor { floor: self.floor, region: self.region, next: self.next }
    }

    /// Hands out `count` frames as one span.
    ///
    /// The allocator walks a rising sequence inside a usable region, so a run
    /// is contiguous until it reaches a gap in the map, which is refused rather
    /// than papered over.
    pub fn run(&mut self, count: u64) -> Result<memory::Span, RunError> {
        if count == 0 {
            return Err(RunError::Empty);
        }
        let first = self.allocate().ok_or(RunError::OutOfFrames)?.start();
        let mut previous = first;
        for _ in 1..count {
            let frame = self.allocate().ok_or(RunError::OutOfFrames)?.start();
            if frame != previous + FRAME_SIZE {
                return Err(RunError::NotContiguous);
            }
            previous = frame;
        }
        memory::Span::frames(first, count).map_err(|_| RunError::OutOfFrames)
    }

    /// Hands out `count` frames as one span, from wherever in the map one fits.
    ///
    /// [`run`](Self::run) takes what is next and refuses a gap; this keeps
    /// looking past it. A firmware map is a list of what is usable and not a
    /// promise about how it is arranged — the region a cursor happens to be
    /// standing in may have four frames left in it — so a caller that needs a
    /// contiguous span and does not care where it sits should ask for one
    /// rather than be told the next four frames are not it.
    ///
    /// What is walked over is spent, the same as a failed [`run`](Self::run)
    /// spends what it took before the gap. That is what makes this a claim and
    /// not a search: it moves the cursor forward and never back.
    pub fn contiguous(&mut self, count: u64) -> Result<memory::Span, RunError> {
        if count == 0 {
            return Err(RunError::Empty);
        }
        let mut first = self.allocate().ok_or(RunError::OutOfFrames)?.start();
        let (mut previous, mut held) = (first, 1);
        while held < count {
            let frame = self.allocate().ok_or(RunError::OutOfFrames)?.start();
            // The frame past a gap is the first of the next candidate and not a
            // frame to give back: a region holding exactly `count` frames is
            // still an answer, and retrying a whole `run` from here would have
            // already spent its first frame on the attempt that found the gap.
            (first, held) =
                if frame == previous + FRAME_SIZE { (first, held + 1) } else { (frame, 1) };
            previous = frame;
        }
        memory::Span::frames(first, count).map_err(|_| RunError::OutOfFrames)
    }

    pub fn allocate(&mut self) -> Option<PhysicalFrame> {
        while self.region < self.map.len() {
            let range = self.map.region(self.region).and_then(|region| {
                // Allocation and direct mapping share this aligned view.
                UsableRange::of(region, self.floor)
            });
            if let Some(range) = range {
                // A lower cursor has not reached this range.
                self.next = self.next.max(range.start());
                let end = self.next.checked_add(FRAME_SIZE)?;
                if end <= range.end() {
                    let frame = PhysicalFrame(self.next);
                    self.next = end;
                    return Some(frame);
                }
            }
            self.region += 1;
            self.next = 0;
        }
        None
    }
}

/// Page-table frames drained at boot for mappings made after the memory map
/// is gone. A fresh [`FrameAllocator`] would reissue frames the live tables
/// already own; this hands each frame out at most once.
pub struct FramePool<const N: usize> {
    frames: [u64; N],
    len: usize,
    next: usize,
}

impl<const N: usize> FramePool<N> {
    pub const fn empty() -> Self {
        Self { frames: [0; N], len: 0, next: 0 }
    }

    /// Drains up to `N` frames. A short fill fails later, at the mapping that
    /// needed the frame.
    pub fn fill(&mut self, frames: &mut FrameAllocator<'_>) -> usize {
        while self.len < N
            && let Some(frame) = frames.allocate()
        {
            self.frames[self.len] = frame.start();
            self.len += 1;
        }
        self.len
    }

    pub fn allocate(&mut self) -> Option<PhysicalFrame> {
        if self.next >= self.len {
            return None;
        }
        let frame = self.frames[self.next];
        self.next += 1;
        Some(PhysicalFrame(frame))
    }

    pub const fn remaining(&self) -> usize {
        self.len - self.next
    }
}

impl<const N: usize> Default for FramePool<N> {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingError {
    WritableExecutable,
    InvalidAddress,
    OutOfFrames,
    Backend,
    /// The address has no translation in the table that was walked.
    Unmapped,
    /// The granted rights do not match what the section is allowed to hold.
    Permissions,
    /// A leaf reaches beyond its declared range.
    Straddling,
    /// A leaf is too coarse for the range's rights boundary.
    Granularity,
    /// A translation exists where the kernel declared no mapping at all.
    Unexpected,
    /// Cacheability does not match the mapped memory.
    Cacheability,
}

/// Rights read from a live translation-table leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageProtection {
    read: bool,
    write: bool,
    execute: bool,
    cache: Cache,
}

impl PageProtection {
    /// Creates rights for ordinary write-back memory.
    pub const fn new(read: bool, write: bool, execute: bool) -> Self {
        Self { read, write, execute, cache: Cache::WriteBack }
    }

    pub const fn cached(mut self, cache: Cache) -> Self {
        self.cache = cache;
        self
    }

    pub const fn cache(self) -> Cache {
        self.cache
    }

    pub const fn is_read(self) -> bool {
        self.read
    }

    pub const fn is_write(self) -> bool {
        self.write
    }

    pub const fn is_execute(self) -> bool {
        self.execute
    }

    pub const fn into_parts(self) -> (bool, bool, bool) {
        (self.read, self.write, self.execute)
    }
}

/// A kernel-image section, named by the rights its pages may hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageSection {
    /// Executable code.
    Text,
    /// Read-only constants.
    Rodata,
    /// Writable data, including `.bss` and the boot stack.
    Data,
}

impl ImageSection {
    /// Checks one section's live rights against the W^X policy.
    pub const fn verify(self, granted: PageProtection) -> Result<(), MappingError> {
        let (read, write, execute) = granted.into_parts();

        if write && execute {
            return Err(MappingError::WritableExecutable);
        }

        let expected = match self {
            Self::Text => read && execute,
            Self::Rodata => read && !write && !execute,
            Self::Data => read && write,
        };
        if expected { Ok(()) } else { Err(MappingError::Permissions) }
    }
}

/// Page permissions that enforce W^X at construction time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapPermissions {
    write: bool,
    execute: bool,
}

impl MapPermissions {
    pub const fn new(write: bool, exec: bool) -> Result<Self, MappingError> {
        if write && exec {
            Err(MappingError::WritableExecutable)
        } else {
            Ok(Self { write, execute: exec })
        }
    }

    pub const fn is_write(self) -> bool {
        self.write
    }

    pub const fn is_execute(self) -> bool {
        self.execute
    }
}

/// A byte-oriented diagnostic console.
pub trait SerialPort {
    fn init(&mut self) {}

    fn write_byte(&mut self, byte: u8);

    fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }
}

/// Adapts a [`SerialPort`] to Rust's formatting machinery.
pub struct SerialWriter<'s, S: SerialPort + ?Sized> {
    serial: &'s mut S,
}

impl<'s, S: SerialPort + ?Sized> SerialWriter<'s, S> {
    pub fn new(serial: &'s mut S) -> Self {
        Self { serial }
    }
}

impl<S: SerialPort + ?Sized> fmt::Write for SerialWriter<'_, S> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.serial.write_bytes(text.as_bytes());
        Ok(())
    }
}

/// Interrupt routing implemented by a concrete architecture crate.
pub trait InterruptController {
    fn init(&mut self) {}
    fn enable_irq(&mut self, irq: u8);
}

/// Terminal state reported by the kernel to its platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    Success,
    Failure,
}

/// Failure while enabling a platform's hardware services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformError {
    Unsupported,
    MissingPhysicalMemoryMap,
    InvalidHardware,
    Mapping(MappingError),
    Fabric(FabricError),
    MissingConfigSpace,
    /// Free RAM could not cover a request for frames.
    Frames(RunError),
    /// A view refused to be opened, filled, or emptied.
    View(view::Error),
}

impl From<RunError> for PlatformError {
    fn from(error: RunError) -> Self {
        Self::Frames(error)
    }
}

impl From<MappingError> for PlatformError {
    fn from(error: MappingError) -> Self {
        Self::Mapping(error)
    }
}

impl From<FabricError> for PlatformError {
    fn from(error: FabricError) -> Self {
        Self::Fabric(error)
    }
}

impl From<view::Error> for PlatformError {
    fn from(error: view::Error) -> Self {
        Self::View(error)
    }
}

/// Hardware services used directly by architecture-independent kernel code.
pub trait Platform: DeviceMapper + InterruptFabric + Local + Smp {
    type Serial: SerialPort;

    fn serial(&mut self) -> &mut Self::Serial;

    fn initialize(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Ok(())
    }

    fn verify_exception_path(&mut self) -> bool {
        false
    }

    fn verify_owned_mapping(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    fn verify_image_protection(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Maps, exercises, and audits an MMIO window from [`Inventory::device`].
    ///
    /// [`Inventory::device`]: memory::Inventory::device
    fn verify_device_window(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// The largest leaf the boot mapping of RAM actually ended up using.
    ///
    /// Read back out of the live tables rather than remembered while building
    /// them: what the mapper meant to do is not evidence that the hardware
    /// translates that way. A platform whose tables cannot be walked returns
    /// [`PlatformError::Unsupported`], and the kernel reports that instead of
    /// a size it did not check.
    fn largest_ram_leaf(
        &mut self,
        _boot_info: &BootInfo<'_>,
    ) -> Result<audit::Leaf, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// The PCI configuration space firmware described, if there is one.
    fn config_space(&mut self, _boot_info: &BootInfo<'_>) -> Result<ConfigSpace, PlatformError> {
        Err(PlatformError::MissingConfigSpace)
    }

    /// Sends every interrupt line this platform raises to `sink`.
    fn route_interrupts(&mut self, _sink: &'static dyn Sink) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// What this machine's translation hardware turned out to be able to do,
    /// once [`initialize`](Self::initialize) has probed it.
    ///
    /// The global VA allocator is cut from the address width and the domain
    /// budget follows from the tag width, so both are asked of the hardware
    /// rather than assumed. A port that has not probed yet returns `None`, and
    /// the kernel hands out no addresses on it.
    fn address_space(&self) -> Option<va::Widths> {
        None
    }

    /// A cursor past the RAM the kernel's own tables and image already own.
    ///
    /// A driver resumes a [`FrameAllocator`] here to back DMA out of frames no
    /// live mapping claims. A platform that cannot say returns `None`, and the
    /// driver goes without.
    ///
    /// The cursor is a snapshot, not a reservation: a later
    /// [`claim_ram`](Self::claim_ram) moves the platform past it, so one taken
    /// before that call names frames somebody else now owns. Read it again
    /// rather than keeping one.
    fn free_frames(&self) -> Option<FrameCursor> {
        None
    }

    /// Hands out `count` frames of that same free RAM, for keeps.
    ///
    /// [`free_frames`](Self::free_frames) only says where the kernel's own
    /// mappings end, so two callers resuming there are handed the same RAM.
    /// This moves the platform's cursor past what it returns, which is what the
    /// heap needs: it never gives its span back.
    ///
    /// So the span is the caller's for the life of the kernel. There is no
    /// giving it back — no free list stands behind this, and the next consumer
    /// of RAM starts where the cursor now is, which is only true because every
    /// claim is recorded here rather than in the caller.
    fn claim_ram(
        &mut self,
        _boot_info: &BootInfo<'_>,
        _count: u64,
    ) -> Result<memory::Span, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Opens an empty view of the one address space, tagged `asid`.
    ///
    /// Empty means empty: the kernel's own text is not in it, which is the only
    /// thing that makes a tier-2 domain a boundary rather than a convention.
    /// See [`view`] for what a view is and why a grant into one costs no copy.
    fn open_view(&mut self, _asid: asid::Asid) -> Result<View, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Makes `extent` reachable from `view`, backed by `span`, with `rights`.
    ///
    /// The grant is at the extent's own class, so a gigabyte-class extent
    /// becomes gigabyte leaves and costs one page-table entry per gigabyte. The
    /// span has to cover the extent and share its alignment; anything else is
    /// [`view::Error::Backing`], because a leaf whose physical base is not
    /// aligned to the leaf size names memory nobody meant to hand over.
    fn grant(
        &mut self,
        _view: View,
        _extent: &va::Extent,
        _span: memory::Span,
        _rights: memory::Rights,
    ) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Takes `extent` back out of `view`, and says how many leaves went.
    ///
    /// This clears the leaves and stops. The flush every core owes and the
    /// return of the addresses to the allocator are the caller's, in that
    /// order, for the reason [`view`] spells out: a core that cached the leaf
    /// before this call still translates through it afterwards.
    fn revoke(&mut self, _view: View, _extent: &va::Extent) -> Result<u64, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// What `view` translates `address` through, read back out of its tables.
    ///
    /// `None` is the honest answer for an address the view cannot reach, which
    /// is what a domain marker is evidence of: not that a grant was intended,
    /// but that the hardware would or would not follow it.
    fn resident(&self, _view: View, _address: u64) -> Option<audit::Leaf> {
        None
    }

    fn terminate(&mut self, status: ExitStatus) -> !;
}

/// Reports a bare-metal panic through the selected platform.
pub fn panic_handler<P>(info: &core::panic::PanicInfo<'_>) -> !
where
    P: Platform + Default,
{
    use core::fmt::Write as _;

    let mut platform = P::default();
    let serial = platform.serial();
    serial.init();
    let _ = writeln!(SerialWriter::new(serial), "MOLT_PANIC: {info}");
    platform.terminate(ExitStatus::Failure)
}
