#![no_std]

//! Hardware-independent contracts shared by the kernel and architecture crates.

pub mod audit;
pub mod memory;

use core::fmt;

pub use crate::memory::Cache;

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
pub const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

/// Rounds `value` up to a multiple of `alignment`, or `None` when that overflows.
///
/// `alignment` must be a power of two.
pub const fn align_up(value: u64, alignment: u64) -> Option<u64> {
    match value.checked_add(alignment - 1) {
        Some(value) => Some(align_down(value, alignment)),
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

/// A resumable [`FrameAllocator`] position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameCursor {
    floor: u64,
    region: usize,
    next: u64,
}

impl<'map> FrameAllocator<'map> {
    pub const fn new(map: &'map dyn MemoryMap) -> Self {
        Self::above(map, 0)
    }

    /// Hands out usable frames at or above `floor`.
    pub const fn above(map: &'map dyn MemoryMap, floor: u64) -> Self {
        Self { map, floor, region: 0, next: 0 }
    }

    /// Resumes allocation over `map` from a cursor an earlier allocator left.
    pub const fn resume(map: &'map dyn MemoryMap, cursor: FrameCursor) -> Self {
        Self { map, floor: cursor.floor, region: cursor.region, next: cursor.next }
    }

    pub const fn cursor(&self) -> FrameCursor {
        FrameCursor { floor: self.floor, region: self.region, next: self.next }
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
}

/// Hardware services used directly by architecture-independent kernel code.
pub trait Platform {
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

    fn arm_timer(&mut self, _initial_count: u32) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    fn monotonic_ticks(&self) -> u64 {
        0
    }

    fn wait_for_timer_change(&mut self, previous: u64) {
        while self.monotonic_ticks() == previous {
            core::hint::spin_loop();
        }
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
