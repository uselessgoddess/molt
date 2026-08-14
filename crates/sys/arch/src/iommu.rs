//! Device-visible addresses and the mappings that keep DMA memory alive.
//!
//! [`Mapping`] owns its [`Region`], so an arena cannot reclaim the bytes while
//! a device may still reach them. The only safe way back to a `Region` is the
//! consuming [`Mapper::unmap`] operation. A driver therefore passes an
//! [`Iova`], never a raw physical address, into a descriptor whether the
//! backend is an [`Identity`] mapper or an isolating IOMMU.

use crate::dma::{DmaError, Region};

/// An address in one device's DMA address space.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Iova(u64);

impl Iova {
    pub const fn new(address: u64) -> Self {
        Self(address)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The requester ID whose DMA address space a mapping belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceId(u32);

impl DeviceId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// What a device may do through a mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaPerm(u8);

impl DmaPerm {
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);

    pub const fn can_read(self) -> bool {
        self.0 & Self::READ.0 != 0
    }

    pub const fn can_write(self) -> bool {
        self.0 & Self::WRITE.0 != 0
    }
}

/// A checked portion of one mapping suitable for a device descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaSlice {
    iova: Iova,
    len: u32,
    writable: bool,
}

impl DmaSlice {
    pub const fn iova(self) -> Iova {
        self.iova
    }

    pub const fn len(self) -> u32 {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub const fn is_writable(self) -> bool {
        self.writable
    }
}

/// A DMA region installed in one device's address space.
///
/// The region remains owned here until a mapper consumes the mapping during
/// unmap. Dropping a mapping leaks its arena claim rather than making live DMA
/// memory reusable; drivers explicitly unmap after stopping their device.
#[derive(Debug)]
pub struct Mapping {
    region: Region,
    device: DeviceId,
    iova: Iova,
    perm: DmaPerm,
}

impl Mapping {
    /// Records a mapping a backend has successfully installed.
    ///
    /// # Safety
    ///
    /// `iova..iova + region.len()` must resolve to `region` for `device` with
    /// exactly `perm` until the same backend successfully unmaps this value.
    pub unsafe fn from_backend(
        region: Region,
        device: DeviceId,
        iova: Iova,
        perm: DmaPerm,
    ) -> Self {
        Self { region, device, iova, perm }
    }

    pub const fn device(&self) -> DeviceId {
        self.device
    }

    pub const fn iova(&self) -> Iova {
        self.iova
    }

    pub const fn perm(&self) -> DmaPerm {
        self.perm
    }

    pub const fn len(&self) -> u64 {
        self.region.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.region.is_empty()
    }

    /// Names bytes the device reads.
    pub fn readable(&self, offset: u64, len: u32) -> Result<DmaSlice, DmaError> {
        if !self.perm.can_read() {
            return Err(DmaError::Permission);
        }
        self.slice(offset, len, false)
    }

    /// Names bytes the device writes.
    pub fn writable(&self, offset: u64, len: u32) -> Result<DmaSlice, DmaError> {
        if !self.perm.can_write() {
            return Err(DmaError::Permission);
        }
        self.slice(offset, len, true)
    }

    pub fn read_u8(&self, offset: u64) -> Result<u8, DmaError> {
        self.region.read_u8(offset)
    }

    pub fn read_u16(&self, offset: u64) -> Result<u16, DmaError> {
        self.region.read_u16(offset)
    }

    pub fn read_u32(&self, offset: u64) -> Result<u32, DmaError> {
        self.region.read_u32(offset)
    }

    pub fn read_u64(&self, offset: u64) -> Result<u64, DmaError> {
        self.region.read_u64(offset)
    }

    pub fn write_u8(&self, offset: u64, value: u8) -> Result<(), DmaError> {
        self.region.write_u8(offset, value)
    }

    pub fn write_u16(&self, offset: u64, value: u16) -> Result<(), DmaError> {
        self.region.write_u16(offset, value)
    }

    pub fn write_u32(&self, offset: u64, value: u32) -> Result<(), DmaError> {
        self.region.write_u32(offset, value)
    }

    pub fn write_u64(&self, offset: u64, value: u64) -> Result<(), DmaError> {
        self.region.write_u64(offset, value)
    }

    pub fn write_from(&self, offset: u64, bytes: &[u8]) -> Result<(), DmaError> {
        self.region.write_from(offset, bytes)
    }

    pub fn read_into(&self, offset: u64, bytes: &mut [u8]) -> Result<(), DmaError> {
        self.region.read_into(offset, bytes)
    }

    pub fn zero(&self) {
        self.region.zero();
    }

    /// Gives the region back after a backend has removed this exact mapping.
    ///
    /// # Safety
    ///
    /// The caller must have made the mapping unreachable by the device and
    /// must not use the saved IOVA again.
    pub unsafe fn into_region(self) -> Region {
        self.region
    }

    fn slice(&self, offset: u64, len: u32, writable: bool) -> Result<DmaSlice, DmaError> {
        let end = offset.checked_add(len as u64).ok_or(DmaError::Address)?;
        if len == 0 || end > self.len() {
            return Err(DmaError::Range);
        }
        let address = self.iova.get().checked_add(offset).ok_or(DmaError::Address)?;
        Ok(DmaSlice { iova: Iova::new(address), len, writable })
    }
}

/// A failed map, retaining the region that never became device-visible.
#[derive(Debug)]
pub struct MapError {
    error: DmaError,
    region: Region,
}

impl MapError {
    pub const fn new(error: DmaError, region: Region) -> Self {
        Self { error, region }
    }

    pub const fn error(&self) -> DmaError {
        self.error
    }

    pub fn into_region(self) -> Region {
        self.region
    }
}

/// A failed unmap, retaining the mapping the device may still reach.
#[derive(Debug)]
pub struct UnmapError {
    error: DmaError,
    mapping: Mapping,
}

impl UnmapError {
    pub const fn new(error: DmaError, mapping: Mapping) -> Self {
        Self { error, mapping }
    }

    pub const fn error(&self) -> DmaError {
        self.error
    }

    pub fn into_mapping(self) -> Mapping {
        self.mapping
    }
}

/// Installs and removes device-scoped DMA mappings.
pub trait Mapper {
    /// Whether devices behind this backend must negotiate ACCESS_PLATFORM.
    fn access_platform(&self) -> bool;

    fn map(&mut self, device: DeviceId, region: Region, perm: DmaPerm)
    -> Result<Mapping, MapError>;

    /// Removes one mapping and returns the region it kept alive.
    ///
    /// Consuming `mapping` prevents a safe caller from unmapping twice:
    ///
    /// ```compile_fail
    /// use molt_arch::iommu::{Mapper, Mapping, UnmapError};
    ///
    /// fn twice<M: Mapper>(mapper: &mut M, mapping: Mapping) -> Result<(), UnmapError> {
    ///     mapper.unmap(mapping)?;
    ///     mapper.unmap(mapping)?;
    ///     Ok(())
    /// }
    /// ```
    fn unmap(&mut self, mapping: Mapping) -> Result<Region, UnmapError>;
}

impl<M: Mapper + ?Sized> Mapper for &mut M {
    fn access_platform(&self) -> bool {
        (**self).access_platform()
    }

    fn map(
        &mut self,
        device: DeviceId,
        region: Region,
        perm: DmaPerm,
    ) -> Result<Mapping, MapError> {
        (**self).map(device, region, perm)
    }

    fn unmap(&mut self, mapping: Mapping) -> Result<Region, UnmapError> {
        (**self).unmap(mapping)
    }
}

/// A backend for machines where the device addresses physical memory directly.
#[derive(Default)]
pub struct Identity;

impl Mapper for Identity {
    fn access_platform(&self) -> bool {
        false
    }

    fn map(
        &mut self,
        device: DeviceId,
        region: Region,
        perm: DmaPerm,
    ) -> Result<Mapping, MapError> {
        let iova = Iova::new(region.physical());
        // SAFETY: identity DMA resolves the physical address carried by the
        // region; ownership keeps it live until `unmap` consumes the mapping.
        Ok(unsafe { Mapping::from_backend(region, device, iova, perm) })
    }

    fn unmap(&mut self, mapping: Mapping) -> Result<Region, UnmapError> {
        // SAFETY: identity mappings require no hardware invalidation. Consuming
        // the mapping is the point after which its address is no longer used.
        Ok(unsafe { mapping.into_region() })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Range {
    start: Iova,
    len: u64,
}

impl Range {
    fn end(self) -> Result<u64, DmaError> {
        self.start.get().checked_add(self.len).ok_or(DmaError::Address)
    }
}

/// A fixed-capacity first-fit allocator for one IOVA aperture.
pub struct IovaSpace<const N: usize> {
    start: Iova,
    end: Iova,
    ranges: [Option<Range>; N],
}

impl<const N: usize> IovaSpace<N> {
    pub const fn new(start: Iova, end: Iova) -> Self {
        Self { start, end, ranges: [None; N] }
    }

    pub fn reserve(&mut self, len: u64, align: u64) -> Result<Iova, DmaError> {
        if len == 0 || align == 0 || !align.is_power_of_two() {
            return Err(DmaError::Alignment);
        }
        let mut candidate = align_up(self.start.get(), align)?;
        loop {
            let end = candidate.checked_add(len).ok_or(DmaError::Address)?;
            if end > self.end.get() {
                return Err(DmaError::OutOfSpace);
            }
            let mut moved = false;
            for range in self.ranges.iter().flatten().copied() {
                let occupied_end = range.end()?;
                if candidate < occupied_end && range.start.get() < end {
                    candidate = align_up(occupied_end, align)?;
                    moved = true;
                    break;
                }
            }
            if !moved {
                return self.reserve_at(Iova::new(candidate), len);
            }
        }
    }

    pub fn reserve_at(&mut self, start: Iova, len: u64) -> Result<Iova, DmaError> {
        if len == 0 {
            return Err(DmaError::Empty);
        }
        let range = Range { start, len };
        let end = range.end()?;
        if start < self.start || end > self.end.get() {
            return Err(DmaError::Range);
        }
        if self.ranges.iter().flatten().copied().any(|live| match live.end() {
            Ok(live_end) => start.get() < live_end && live.start.get() < end,
            Err(_) => true,
        }) {
            return Err(DmaError::Overlap);
        }
        let free =
            self.ranges.iter_mut().find(|slot| slot.is_none()).ok_or(DmaError::OutOfSpace)?;
        *free = Some(range);
        Ok(start)
    }

    pub fn release(&mut self, start: Iova, len: u64) -> Result<(), DmaError> {
        let slot = self
            .ranges
            .iter_mut()
            .find(|slot| slot.is_some_and(|range| range.start == start && range.len == len))
            .ok_or(DmaError::NotMapped)?;
        *slot = None;
        Ok(())
    }
}

fn align_up(value: u64, align: u64) -> Result<u64, DmaError> {
    crate::align_up(value, align).ok_or(DmaError::Address)
}

#[derive(Clone, Copy)]
struct FakeEntry {
    device: DeviceId,
    iova: Iova,
    physical: u64,
    len: u64,
    perm: DmaPerm,
}

/// An in-memory translating backend used to exercise isolation without I/O.
pub struct Fake<const N: usize> {
    space: IovaSpace<N>,
    entries: [Option<FakeEntry>; N],
}

impl<const N: usize> Fake<N> {
    pub const fn new(start: Iova, end: Iova) -> Self {
        Self { space: IovaSpace::new(start, end), entries: [None; N] }
    }

    /// Resolves one device access the way an IOMMU would.
    pub fn translate(
        &self,
        device: DeviceId,
        iova: Iova,
        len: u64,
        write: bool,
    ) -> Result<u64, DmaError> {
        let end = iova.get().checked_add(len).ok_or(DmaError::Address)?;
        let entry = self.entries.iter().flatten().find(|entry| {
            entry.device == device
                && iova >= entry.iova
                && entry
                    .iova
                    .get()
                    .checked_add(entry.len)
                    .is_some_and(|mapped_end| end <= mapped_end)
        });
        let entry = entry.ok_or(DmaError::NotMapped)?;
        if (write && !entry.perm.can_write()) || (!write && !entry.perm.can_read()) {
            return Err(DmaError::Permission);
        }
        entry.physical.checked_add(iova.get() - entry.iova.get()).ok_or(DmaError::Address)
    }
}

impl<const N: usize> Mapper for Fake<N> {
    fn access_platform(&self) -> bool {
        true
    }

    fn map(
        &mut self,
        device: DeviceId,
        region: Region,
        perm: DmaPerm,
    ) -> Result<Mapping, MapError> {
        let iova = match self.space.reserve(region.len(), 1) {
            Ok(iova) => iova,
            Err(error) => return Err(MapError::new(error, region)),
        };
        let free = self.entries.iter_mut().find(|entry| entry.is_none());
        let Some(free) = free else {
            self.space.release(iova, region.len()).ok();
            return Err(MapError::new(DmaError::OutOfSpace, region));
        };
        *free =
            Some(FakeEntry { device, iova, physical: region.physical(), len: region.len(), perm });
        // SAFETY: the entry just installed resolves this exact range until the
        // consuming `unmap` below removes it.
        Ok(unsafe { Mapping::from_backend(region, device, iova, perm) })
    }

    fn unmap(&mut self, mapping: Mapping) -> Result<Region, UnmapError> {
        let slot = self.entries.iter_mut().find(|entry| {
            entry.is_some_and(|entry| {
                entry.device == mapping.device()
                    && entry.iova == mapping.iova()
                    && entry.len == mapping.len()
                    && entry.perm == mapping.perm()
            })
        });
        let Some(slot) = slot else {
            return Err(UnmapError::new(DmaError::NotMapped, mapping));
        };
        if let Err(error) = self.space.release(mapping.iova(), mapping.len()) {
            return Err(UnmapError::new(error, mapping));
        }
        *slot = None;
        // SAFETY: the fake translation entry and its IOVA reservation are gone.
        Ok(unsafe { mapping.into_region() })
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceId, DmaPerm, Fake, Identity, Iova, IovaSpace, Mapper};
    use crate::dma::{DmaError, Region};

    fn region(bytes: &mut [u8], physical: u64) -> Region {
        // SAFETY: each test keeps this unique slice alive through unmap.
        unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) }
    }

    #[test]
    fn iova_overlap() -> Result<(), DmaError> {
        let mut space = IovaSpace::<3>::new(Iova::new(0x1000), Iova::new(0x5000));
        space.reserve_at(Iova::new(0x2000), 0x1000)?;

        assert_eq!(space.reserve_at(Iova::new(0x2800), 0x1000), Err(DmaError::Overlap));
        Ok(())
    }

    #[test]
    fn iova_reuse() -> Result<(), DmaError> {
        let mut space = IovaSpace::<2>::new(Iova::new(0x1000), Iova::new(0x5000));
        let first = space.reserve(0x1000, 0x1000)?;
        space.release(first, 0x1000)?;

        assert_eq!(space.reserve(0x1000, 0x1000)?, first);
        assert_eq!(space.release(first, 0x1000), Ok(()));
        assert_eq!(space.release(first, 0x1000), Err(DmaError::NotMapped));
        Ok(())
    }

    #[test]
    fn fake_scope() -> Result<(), DmaError> {
        let mut bytes = [0u8; 32];
        let mut fake = Fake::<2>::new(Iova::new(0x8000), Iova::new(0x9000));
        let owner = DeviceId::new(7);
        let mapping = fake
            .map(owner, region(&mut bytes, 0x20_0000), DmaPerm::READ)
            .map_err(|error| error.error())?;

        assert_eq!(fake.translate(owner, mapping.iova(), 4, false)?, 0x20_0000);
        assert_eq!(
            fake.translate(DeviceId::new(8), mapping.iova(), 4, false),
            Err(DmaError::NotMapped)
        );
        assert_eq!(fake.translate(owner, mapping.iova(), 4, true), Err(DmaError::Permission));
        assert_eq!(mapping.writable(0, 4), Err(DmaError::Permission));

        let region = fake.unmap(mapping).map_err(|error| error.error())?;
        assert_eq!(fake.translate(owner, Iova::new(0x8000), 1, false), Err(DmaError::NotMapped));
        assert_eq!(region.len(), 32);
        Ok(())
    }

    #[test]
    fn identity_iova() -> Result<(), DmaError> {
        let mut bytes = [0u8; 16];
        let mut identity = Identity;
        let mapping = identity
            .map(DeviceId::new(1), region(&mut bytes, 0xab00), DmaPerm::READ_WRITE)
            .map_err(|error| error.error())?;

        assert_eq!(mapping.iova(), Iova::new(0xab00));
        assert_eq!(mapping.readable(4, 8)?.iova(), Iova::new(0xab04));
        assert_eq!(mapping.writable(12, 8), Err(DmaError::Range));
        identity.unmap(mapping).map_err(|error| error.error())?;
        Ok(())
    }
}
