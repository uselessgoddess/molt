//! One split virtqueue, laid over three DMA regions the device shares.
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

use molt_arch::dma::Region;

use crate::VirtioError;

/// The largest queue this driver builds. A read is three descriptors, so a
/// handful of slots keeps several requests in flight without a heap.
pub const MAX_SIZE: u16 = 8;

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

/// One buffer a descriptor points at: a physical range the device may read, or
/// read and write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    physical: u64,
    len: u32,
    writable: bool,
}

impl Segment {
    /// A segment the device only reads, such as a request header.
    pub const fn readable(physical: u64, len: u32) -> Self {
        Self { physical, len, writable: false }
    }

    /// A segment the device writes into, such as a data or status buffer.
    pub const fn writable(physical: u64, len: u32) -> Self {
        Self { physical, len, writable: true }
    }
}

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
    descriptors: Region,
    driver: Region,
    device: Region,
    size: u16,
    free: [u16; MAX_SIZE as usize],
    available: u16,
    avail_idx: u16,
    used_seen: u16,
}

impl Queue {
    /// Lays a queue of `size` descriptors over its three regions.
    ///
    /// `size` must be a power of two no larger than `MAX_SIZE`, and each
    /// region must be large enough for its structure; anything else is a
    /// programming error the device would turn into silent corruption.
    pub fn new(
        size: u16,
        descriptors: Region,
        driver: Region,
        device: Region,
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

    pub fn descriptors_physical(&self) -> u64 {
        self.descriptors.physical()
    }

    pub fn driver_physical(&self) -> u64 {
        self.driver.physical()
    }

    pub fn device_physical(&self) -> u64 {
        self.device.physical()
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
            if segment.writable {
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
        self.descriptors.write_u64(at, segment.physical)?;
        self.descriptors.write_u32(at + 8, segment.len)?;
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
        let mut index = head;
        for _ in 0..self.size {
            if index >= self.size {
                return Err(VirtioError::Device);
            }
            let flags = self.descriptors.read_u16(index as u64 * DESCRIPTOR + 12)?;
            let next = self.descriptors.read_u16(index as u64 * DESCRIPTOR + 14)?;
            self.free[self.available as usize] = index;
            self.available += 1;
            if flags & flag::NEXT == 0 {
                return Ok(());
            }
            index = next;
        }
        Err(VirtioError::Device)
    }
}

#[cfg(test)]
mod tests {
    use molt_arch::dma::Region;

    use super::{Queue, Segment, Used, device_bytes, driver_bytes};

    /// A region over a plain buffer, addressed at a fake physical base.
    fn region(bytes: &mut [u8], physical: u64) -> Region {
        // SAFETY: the slice outlives the borrow, is uniquely borrowed, and no
        // other region is handed out over it.
        unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) }
    }

    fn queue(descriptors: &mut [u8], driver: &mut [u8], device: &mut [u8]) -> Queue {
        Queue::new(4, region(descriptors, 0x1000), region(driver, 0x2000), region(device, 0x3000))
            .expect("a legal four-slot queue")
    }

    #[test]
    fn push_chain_segments_and_publish_head() {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);

        let head = queue
            .push(&[Segment::readable(0xaa00, 16), Segment::writable(0xbb00, 512)])
            .expect("room for a two-segment chain");

        assert_eq!(head, 0);
        assert_eq!(&d[12..14], &1u16.to_le_bytes(), "head lacked the NEXT flag");
        assert_eq!(&d[16 + 12..16 + 14], &2u16.to_le_bytes(), "tail was not device-writable");
        assert_eq!(&a[2..4], &1u16.to_le_bytes(), "available index did not advance");
    }

    #[test]
    fn full_queue_refuses_next_chain() {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);

        for _ in 0..4 {
            queue.push(&[Segment::readable(0, 8)]).expect("a free descriptor");
        }

        assert_eq!(queue.push(&[Segment::readable(0, 8)]).err(), Some(super::VirtioError::Full));
    }

    #[test]
    fn pop_free_descriptors() {
        let (mut d, mut a, mut u) = ([0u8; 64], [0u8; 16], [0u8; 64]);
        let mut queue = queue(&mut d, &mut a, &mut u);
        let head = queue
            .push(&[Segment::readable(0xaa00, 16), Segment::writable(0xbb00, 512)])
            .expect("room for a two-segment chain");

        queue.device.write_u32(4, head as u32).expect("the used ring's first element");
        queue.device.write_u32(8, 512).expect("the element's written length");
        queue.device.write_u16(2, 1).expect("the used index");

        assert_eq!(queue.pop(), Ok(Some(Used { head: 0, len: 512 })));
        assert_eq!(queue.available(), 4, "the completed chain was not reclaimed");
    }

    #[test]
    fn region_sizes_match_helpers() {
        assert_eq!(driver_bytes(4), 14);
        assert_eq!(device_bytes(4), 38);
    }
}
