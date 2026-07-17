#![no_std]

//! Hardware-independent contracts shared by the kernel and architecture crates.

use core::fmt;

/// Architecture-neutral information passed from a platform boot adapter.
#[derive(Clone, Copy)]
pub struct BootInfo<'boot> {
    memory_map: &'boot dyn MemoryMap,
    physical_memory_offset: Option<u64>,
}

impl<'boot> BootInfo<'boot> {
    pub const fn new(
        memory_map: &'boot dyn MemoryMap,
        physical_memory_offset: Option<u64>,
    ) -> Self {
        Self { memory_map, physical_memory_offset }
    }

    pub const fn memory_map(&self) -> &'boot dyn MemoryMap {
        self.memory_map
    }

    pub const fn physical_memory_offset(&self) -> Option<u64> {
        self.physical_memory_offset
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

/// Allocation-free iterator over firmware regions marked usable.
pub struct FrameAllocator<'map> {
    map: &'map dyn MemoryMap,
    region: usize,
    next: u64,
}

impl<'map> FrameAllocator<'map> {
    pub const fn new(map: &'map dyn MemoryMap) -> Self {
        Self { map, region: 0, next: 0 }
    }

    pub fn allocate(&mut self) -> Option<PhysicalFrame> {
        while self.region < self.map.len() {
            let Some(region) = self.map.region(self.region) else {
                self.region += 1;
                self.next = 0;
                continue;
            };
            if region.kind() != MemoryRegionKind::Usable {
                self.region += 1;
                self.next = 0;
                continue;
            }
            if self.next == 0 {
                self.next = align_up(region.start(), FRAME_SIZE)?;
            }
            let end = self.next.checked_add(FRAME_SIZE)?;
            if end <= region.end() {
                let frame = PhysicalFrame(self.next);
                self.next = end;
                return Some(frame);
            }
            self.region += 1;
            self.next = 0;
        }
        None
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value.checked_add(alignment - 1).map(|value| value & !(alignment - 1))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingError {
    WritableExecutable,
    InvalidAddress,
    OutOfFrames,
    Backend,
}

/// Page permissions that enforce W^X at construction time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapPermissions {
    writable: bool,
    executable: bool,
}

impl MapPermissions {
    pub const fn new(writable: bool, executable: bool) -> Result<Self, MappingError> {
        if writable && executable {
            Err(MappingError::WritableExecutable)
        } else {
            Ok(Self { writable, executable })
        }
    }

    pub const fn is_writable(self) -> bool {
        self.writable
    }

    pub const fn is_executable(self) -> bool {
        self.executable
    }
}

/// A byte-oriented diagnostic console.
pub trait SerialPort {
    fn init(&mut self) {}
    fn write_byte(&mut self, byte: u8);
}

/// Adapts a [`SerialPort`] to Rust's formatting machinery.
pub struct SerialWriter<'serial, S: SerialPort + ?Sized> {
    serial: &'serial mut S,
}

impl<'serial, S: SerialPort + ?Sized> SerialWriter<'serial, S> {
    pub fn new(serial: &'serial mut S) -> Self {
        Self { serial }
    }
}

impl<S: SerialPort + ?Sized> fmt::Write for SerialWriter<'_, S> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            self.serial.write_byte(byte);
        }
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

/// Hardware services used directly by architecture-independent kernel code.
pub trait Platform {
    type Serial: SerialPort;

    fn serial(&mut self) -> &mut Self::Serial;
    fn terminate(&mut self, status: ExitStatus) -> !;
}
