//! One split virtqueue, laid over three mapped DMA regions the device shares.
//!
//! A queue is a descriptor table plus two rings: the driver publishes
//! descriptor chains through the *available* ring and the device returns them
//! through the *used* ring. [`push`](Queue::push) hands the device a chain and
//! [`pop`](Queue::pop) reclaims one, with the release/acquire fences the
//! specification requires around the two index writes so the device never sees
//! a descriptor before the bytes it points at.
//!
//! The free-descriptor list is a fixed stack, so the queue allocates nothing.
//! That caps a queue at [`MAX_SIZE`] descriptors, which is ample for a block
//! driver whose deepest request is three.

use core::sync::atomic::{Ordering, fence};

use molt_arch::dma::DmaError;
use molt_arch::iommu::{DmaSlice, Iova, Mapping};

use crate::VirtioError;

/// The largest queue this driver builds. A read is three descriptors, so a
/// handful of slots keeps several requests in flight without a heap.
pub const MAX_SIZE: u16 = 32;

/// Descriptor flags (§2.7.1).
mod flag {
    pub const NEXT: u16 = 1;
    pub const WRITE: u16 = 2;
}

/// One descriptor is sixteen bytes: `addr`, `len`, `flags`, `next`.
const DESCRIPTOR: u64 = 16;

/// The bytes a descriptor table of `size` entries needs.
pub const fn descriptor_bytes(size: u16) -> u64 {
    size as u64 * DESCRIPTOR
}

/// The bytes an available ring of `size` entries needs: two `u16` headers, the
/// ring, and the trailing `used_event`.
pub const fn driver_bytes(size: u16) -> u64 {
    4 + 2 * size as u64 + 2
}

/// The bytes a used ring of `size` entries needs: two `u16` headers, the
/// eight-byte elements, and the trailing `avail_event`.
pub const fn device_bytes(size: u16) -> u64 {
    4 + 8 * size as u64 + 2
}

/// One checked mapping slice a descriptor points at.
pub type Segment = DmaSlice;

/// One completed chain: the head descriptor the device returned and how many
/// bytes it wrote.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Used {
    head: u16,
    len: u32,
}

impl Used {
    /// The head descriptor index of the completed chain.
    pub const fn head(self) -> u16 {
        self.head
    }

    /// How many bytes the device reported writing.
    pub const fn written(self) -> u32 {
        self.len
    }
}

/// A split virtqueue over its three regions.
pub struct Queue {
    descriptors: Mapping,
    driver: Mapping,
    device: Mapping,
    size: u16,
    free: [u16; MAX_SIZE as usize],
    available: u16,
    avail_idx: u16,
    used_seen: u16,
}

impl Queue {
    /// Lays a queue of `size` descriptors over its three regions.
    ///
    /// `size` must be a power of two no larger than `MAX_SIZE`, each region
    /// must be large enough for its structure, and the mappings must grant the
    /// device the access each split-ring role needs. In particular, a device
    /// both reads and writes its used-ring index while maintaining the ring.
    pub fn new(
        size: u16,
        descriptors: Mapping,
        driver: Mapping,
        device: Mapping,
    ) -> Result<Self, VirtioError> {
        if size == 0 || size > MAX_SIZE || !size.is_power_of_two() {
            return Err(VirtioError::Device);
        }
        if descriptors.len() < descriptor_bytes(size)
            || driver.len() < driver_bytes(size)
            || device.len() < device_bytes(size)
        {
            return Err(VirtioError::Device);
        }
        if !descriptors.perm().can_read()
            || !driver.perm().can_read()
            || !device.perm().can_read()
            || !device.perm().can_write()
        {
            return Err(VirtioError::Dma(DmaError::Permission));
        }
        descriptors.zero();
        driver.zero();
        device.zero();

        // A stack whose top is `free[available - 1]`. Descending order puts
        // descriptor zero on top, so the first chain starts at a tidy head.
        let mut free = [0u16; MAX_SIZE as usize];
        for slot in 0..size {
            free[slot as usize] = size - 1 - slot;
        }
        Ok(Self {
            descriptors,
            driver,
            device,
            size,
            free,
            available: size,
            avail_idx: 0,
            used_seen: 0,
        })
    }

    pub const fn size(&self) -> u16 {
        self.size
    }

    /// How many descriptors are free to be pushed.
    pub const fn available(&self) -> u16 {
        self.available
    }

    pub fn descriptors_iova(&self) -> Iova {
        self.descriptors.iova()
    }

    pub fn driver_iova(&self) -> Iova {
        self.driver.iova()
    }

    pub fn device_iova(&self) -> Iova {
        self.device.iova()
    }

    /// Gives up the three mappings, for a caller that has stopped the device.
    pub fn mappings(self) -> [Mapping; 3] {
        [self.descriptors, self.driver, self.device]
    }

    /// Publishes `segments` as one descriptor chain and returns its head.
    ///
    /// Returns [`VirtioError::Full`] when the chain will not fit in the free
    /// descriptors — the backpressure signal a caller drains completions
    /// against rather than overrunning the ring.
    pub fn push(&mut self, segments: &[Segment]) -> Result<u16, VirtioError> {
        let count = segments.len() as u16;
        if count == 0 || count > self.available {
            return Err(VirtioError::Full);
        }

        // Reserve the whole chain before writing any of it, so a short free
        // list never leaves a half-linked chain behind. Descriptors come off
        // the top of the stack, so the chain runs `free[top], free[top-1], ...`.
        let top = self.available - 1;
        let head = self.free[top as usize];
        for (offset, segment) in segments.iter().enumerate() {
            let offset = offset as u16;
            let index = self.free[(top - offset) as usize];
            let last = offset + 1 == count;
            let next = if last { 0 } else { self.free[(top - offset - 1) as usize] };
            let mut flags = 0;
            if segment.is_writable() {
                flags |= flag::WRITE;
            }
            if !last {
                flags |= flag::NEXT;
            }
            self.write_descriptor(index, segment, flags, next)?;
        }
        self.available -= count;

        let slot = self.avail_idx % self.size;
        self.driver.write_u16(4 + 2 * slot as u64, head)?;

        // The descriptors and their buffers must be visible before the index
        // that publishes them; the device reads the index and follows back.
        fence(Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.driver.write_u16(2, self.avail_idx)?;
        Ok(head)
    }

    /// Reclaims one completed chain, or `None` if the device has returned
    /// nothing new.
    pub fn pop(&mut self) -> Result<Option<Used>, VirtioError> {
        let device_idx = self.device.read_u16(2)?;
        // The index is read before the element it guards; pairing this acquire
        // with the device's release keeps the read of `id`/`len` from moving
        // ahead of the index that made them valid.
        fence(Ordering::Acquire);
        if device_idx == self.used_seen {
            return Ok(None);
        }

        let slot = self.used_seen % self.size;
        let element = 4 + 8 * slot as u64;
        let head = self.device.read_u32(element)? as u16;
        let len = self.device.read_u32(element + 4)?;

        self.free_chain(head)?;
        self.used_seen = self.used_seen.wrapping_add(1);
        Ok(Some(Used { head, len }))
    }

    fn write_descriptor(
        &self,
        index: u16,
        segment: &Segment,
        flags: u16,
        next: u16,
    ) -> Result<(), VirtioError> {
        let at = index as u64 * DESCRIPTOR;
        self.descriptors.write_u64(at, segment.iova().get())?;
        self.descriptors.write_u32(at + 8, segment.len())?;
        self.descriptors.write_u16(at + 12, flags)?;
        self.descriptors.write_u16(at + 14, next)?;
        Ok(())
    }

    /// Returns a chain's descriptors to the free list, following `NEXT` links.
    ///
    /// The walk is bounded by the queue size: a device that returns a chain
    /// longer than the table describes a cycle, which is refused rather than
    /// followed forever.
    fn free_chain(&mut self, head: u16) -> Result<(), VirtioError> {
        let mut chain = [0u16; MAX_SIZE as usize];
        let mut seen = 0u32;
        let mut count = 0usize;
        let mut index = head;
        for _ in 0..self.size {
            if index >= self.size {
                return Err(VirtioError::Device);
            }
            let bit = 1u32 << index;
            if seen & bit != 0 {
                return Err(VirtioError::Device);
            }
            seen |= bit;
            chain[count] = index;
            count += 1;
            let flags = self.descriptors.read_u16(index as u64 * DESCRIPTOR + 12)?;
            let next = self.descriptors.read_u16(index as u64 * DESCRIPTOR + 14)?;
            if flags & flag::NEXT == 0 {
                if self.available as usize + count > self.size as usize {
                    return Err(VirtioError::Device);
                }
                for index in chain[..count].iter().copied() {
                    self.free[self.available as usize] = index;
                    self.available += 1;
                }
                return Ok(());
            }
            index = next;
        }
        Err(VirtioError::Device)
    }
}

#[cfg(test)]
mod tests {
    use molt_arch::dma::{DmaError, Region};
    use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Mapper, Mapping};

    use super::{Queue, Used, device_bytes, driver_bytes};
    use crate::VirtioError;

    /// A region over a plain buffer, addressed at a fake physical base.
    fn region(bytes: &mut [u8], physical: u64) -> Region {
        // SAFETY: the slice outlives the borrow, is uniquely borrowed, and no
        // other region is handed out over it.
        unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) }
    }

    fn mapping(bytes: &mut [u8], physical: u64, perm: DmaPerm) -> Mapping {
        Identity.map(DeviceId::new(0), region(bytes, physical), perm).ok().unwrap()
    }

    fn queue(descriptors: &mut [u8], driver: &mut [u8], device: &mut [u8]) -> Queue {
        Queue::new(
            4,
            mapping(descriptors, 0x1000, DmaPerm::READ),
            mapping(driver, 0x2000, DmaPerm::READ),
            mapping(device, 0x3000, DmaPerm::READ_WRITE),
        )
        .unwrap()
    }

    #[test]
    fn used_perm() {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);

        let result = Queue::new(
            4,
            mapping(&mut d, 0x1000, DmaPerm::READ),
            mapping(&mut a, 0x2000, DmaPerm::READ),
            mapping(&mut u, 0x3000, DmaPerm::WRITE),
        );

        assert!(matches!(result, Err(VirtioError::Dma(DmaError::Permission))));
    }

    #[test]
    fn push_chain_segments_and_publish_head() -> Result<(), VirtioError> {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);

        let mut header = [0u8; 16];
        let mut data = [0u8; 512];
        let header = mapping(&mut header, 0xaa00, DmaPerm::READ);
        let data = mapping(&mut data, 0xbb00, DmaPerm::WRITE);
        let head = queue.push(&[header.readable(0, 16)?, data.writable(0, 512)?])?;

        assert_eq!(head, 0);
        assert_eq!(&d[12..14], &1u16.to_le_bytes(), "head lacked the NEXT flag");
        assert_eq!(&d[16 + 12..16 + 14], &2u16.to_le_bytes(), "tail was not device-writable");
        assert_eq!(&a[2..4], &1u16.to_le_bytes(), "available index did not advance");
        Ok(())
    }

    #[test]
    fn full_queue_refuses_next_chain() -> Result<(), VirtioError> {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);

        let mut bytes = [0u8; 32];
        let buffers = mapping(&mut bytes, 0xaa00, DmaPerm::READ);
        for offset in [0, 8, 16, 24] {
            queue.push(&[buffers.readable(offset, 8)?])?;
        }

        assert_eq!(queue.push(&[buffers.readable(0, 8)?]).err(), Some(super::VirtioError::Full));
        Ok(())
    }

    #[test]
    fn pop_free_descriptors() -> Result<(), VirtioError> {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);
        let mut header = [0u8; 16];
        let mut data = [0u8; 512];
        let header = mapping(&mut header, 0xaa00, DmaPerm::READ);
        let data = mapping(&mut data, 0xbb00, DmaPerm::WRITE);
        let head = queue.push(&[header.readable(0, 16)?, data.writable(0, 512)?])?;

        queue.device.write_u32(4, head as u32)?;
        queue.device.write_u32(8, 512)?;
        queue.device.write_u16(2, 1)?;

        assert_eq!(queue.pop(), Ok(Some(Used { head: 0, len: 512 })));
        assert_eq!(queue.available(), 4, "the completed chain was not reclaimed");
        Ok(())
    }

    #[test]
    fn cyclic_used() -> Result<(), VirtioError> {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);
        let mut bytes = [0u8; 16];
        let buffers = mapping(&mut bytes, 0xaa00, DmaPerm::READ);
        let head = queue.push(&[buffers.readable(0, 8)?, buffers.readable(8, 8)?])?;
        // Make the second descriptor point back to the head.
        queue.descriptors.write_u16(16 + 12, 1)?;
        queue.descriptors.write_u16(16 + 14, head)?;
        queue.device.write_u32(4, head as u32)?;
        queue.device.write_u16(2, 1)?;

        assert_eq!(queue.pop(), Err(VirtioError::Device));
        assert_eq!(queue.available(), 2, "a malformed chain freed live descriptors");
        Ok(())
    }

    #[test]
    fn region_sizes_match_helpers() {
        assert_eq!(driver_bytes(4), 14);
        assert_eq!(device_bytes(4), 38);
    }
}
