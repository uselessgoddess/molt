//! The block driver: the handshake that brings a device up, sector I/O with a
//! durable flush, and the reset that reclaims its frames.
//!
//! [`Block::start`] runs the modern initialization sequence, requires cache
//! flush support, routes queue zero through an MSI-X vector, and programs it
//! out of an [`Arena`] of device-owned frames. Each command waits on
//! [`Arrivals`] for the interrupt that says the used ring moved, giving up with
//! [`VirtioError::Timeout`] when the line stays quiet, so a wedged device
//! cannot hang the caller. [`Block::reset`] stops the device *before* it hands
//! the frames back, so no in-flight DMA can land in a reclaimed frame.
//!
//! [`molt_block::Device`] is how anything above reaches this: the filesystem
//! reads sectors, not virtqueues, and gets the same contract from a loopback
//! image.

use molt_arch::Mmio;
use molt_arch::dma::{Arena, DmaError, Region};
use molt_block::{BlockError, Device, Disk};

use crate::VirtioError;
use crate::config::{Common, status};
use crate::interrupt::Arrivals;
use crate::notify::Notify;
use crate::queue::{self, Queue, Segment};
use crate::request::{Completion, Requests};

/// A block read request (`VIRTIO_BLK_T_IN`).
const VIRTIO_BLK_T_IN: u32 = 0;

/// A block write request (`VIRTIO_BLK_T_OUT`).
const VIRTIO_BLK_T_OUT: u32 = 1;

/// A cache flush request (`VIRTIO_BLK_T_FLUSH`).
const VIRTIO_BLK_T_FLUSH: u32 = 4;

/// The device is read-only (`VIRTIO_BLK_F_RO`).
const VIRTIO_BLK_F_RO: u64 = 1 << 5;

/// The device accepts cache flush requests (`VIRTIO_BLK_F_FLUSH`).
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

/// The status byte a device writes on success (`VIRTIO_BLK_S_OK`).
const VIRTIO_BLK_S_OK: u8 = 0;

/// Where the block device's capacity, in sectors, sits in its configuration
/// structure (§5.2.4).
const CAPACITY_AT: u64 = 0;

/// How many times a capacity read retries a device that changes its
/// configuration underneath it.
const CONFIG_SPINS: u32 = 16;

/// The request header the device reads: type, reserved, sector.
const HEADER_LEN: u32 = 16;

/// Where the one-byte status sits in the control region, past the header.
const STATUS_AT: u64 = HEADER_LEN as u64;

/// The control region holds the header and the trailing status byte.
const CONTROL_BYTES: u64 = HEADER_LEN as u64 + 1;

/// The data region is one frame, which bounds a single transfer.
const DATA_BYTES: u64 = 4096;

/// The largest transfer the driver issues as one request.
const TRANSFER: usize = DATA_BYTES as usize;

/// A VirtIO block device driven through one queue of frames it owns.
pub struct Block<'slots, 'window, A> {
    common: Common<'window>,
    notify: Notify<'window>,
    queue: Queue,
    requests: Requests<{ queue::MAX_SIZE as usize }>,
    arrivals: A,
    control: Region,
    data: Region,
    arena: Arena<'slots>,
    notify_off: u16,
    capacity: u64,
}

impl<'slots, 'window, A: Arrivals> Block<'slots, 'window, A> {
    /// Brings a device up over its `common`, `notify`, and `device` windows,
    /// allocating every ring and buffer from `arena`.
    ///
    /// Runs the modern handshake, refuses read-only devices, requires durable
    /// flush support, and programs queue zero onto `vector`, which the caller
    /// must already have routed and enabled — `arrivals` is the line it lands
    /// on. A device that offers no usable queue, will not take the vector, or
    /// rejects the feature set, is refused rather than left half-initialized.
    pub fn start(
        common: Mmio<'window>,
        notify: Mmio<'window>,
        device: Mmio<'window>,
        notify_multiplier: u32,
        vector: u16,
        arrivals: A,
        mut arena: Arena<'slots>,
    ) -> Result<Self, VirtioError> {
        let mut common = Common::new(common);
        common.reset()?;
        common.add_status(status::ACKNOWLEDGE)?;
        common.add_status(status::DRIVER)?;
        let features = common.negotiate(VIRTIO_BLK_F_RO | VIRTIO_BLK_F_FLUSH)?;
        if features & VIRTIO_BLK_F_RO != 0 {
            return Err(VirtioError::ReadOnly);
        }
        if features & VIRTIO_BLK_F_FLUSH == 0 {
            return Err(VirtioError::Features);
        }

        // The capacity is only meaningful once the features are settled.
        let capacity = capacity(&common, &device)?;

        common.select_queue(0)?;
        let size = clamp_queue(common.queue_size()?)?;

        let descriptors = arena.region(queue::descriptor_bytes(size))?;
        let driver = arena.region(queue::driver_bytes(size))?;
        let device = arena.region(queue::device_bytes(size))?;
        let control = arena.region(CONTROL_BYTES)?;
        let data = arena.region(DATA_BYTES)?;

        let queue = Queue::new(size, descriptors, driver, device)?;
        common.set_queue_size(size)?;
        common.set_queue_vector(vector)?;
        common.set_queue_rings(
            queue.descriptors_physical(),
            queue.driver_physical(),
            queue.device_physical(),
        )?;
        common.enable_queue()?;
        let notify_off = common.queue_notify_off()?;

        // The queue is programmed, so the device may run.
        common.add_status(status::DRIVER_OK)?;

        Ok(Self {
            common,
            notify: Notify::new(notify, notify_multiplier),
            queue,
            requests: Requests::new(),
            arrivals,
            control,
            data,
            arena,
            notify_off,
            capacity,
        })
    }

    /// How many sectors the device reports holding.
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Runs one request and waits for its status byte.
    ///
    /// Submits a read, write, or two-descriptor flush chain, kicks the device,
    /// and sleeps on its vector until the used ring moves. One arrival can
    /// cover several used entries, so each drains the ring rather than
    /// assuming one entry per interrupt. A device whose line stays quiet has
    /// its request cancelled — the slot stays reserved until the device
    /// returns it — and the command fails with [`VirtioError::Timeout`].
    fn command(
        &mut self,
        request: u32,
        sector: u64,
        data: Option<Segment>,
    ) -> Result<(), VirtioError> {
        self.control.write_u32(0, request)?;
        self.control.write_u32(4, 0)?;
        self.control.write_u64(8, sector)?;
        // Poison the status so a device that answers without writing it is
        // caught rather than read as success.
        self.control.write_u8(STATUS_AT, 0xff)?;

        let header = Segment::readable(self.control.physical(), HEADER_LEN);
        let status = Segment::writable(self.control.physical() + STATUS_AT, 1);
        let head = match data {
            Some(data) => self.queue.push(&[header, data, status])?,
            None => self.queue.push(&[header, status])?,
        };
        let token = self.requests.issue(head);
        self.notify.signal(0, self.notify_off)?;

        while self.arrivals.wait() != 0 {
            while let Some(used) = self.queue.pop()? {
                if let Completion::Delivered = self.requests.complete(used.head()) {
                    if self.control.read_u8(STATUS_AT)? != VIRTIO_BLK_S_OK {
                        return Err(VirtioError::Device);
                    }
                    return Ok(());
                }
            }
        }

        self.requests.cancel(token);
        Err(VirtioError::Timeout)
    }

    /// Resets the device and reclaims every frame the arena handed out.
    ///
    /// The reset comes first so the device stops touching the rings and buffers
    /// before the frames behind them return to the table.
    pub fn reset(self) -> Result<(), VirtioError> {
        let Self { mut common, queue, control, data, arena, .. } = self;
        common.reset()?;
        Ok(reclaim(arena, queue.regions().into_iter().chain([control, data]))?)
    }
}

/// Hands `regions` back and drops the arena's whole span behind them.
///
/// The reset runs whatever a release does, because by here the device is
/// already stopped: returning early on a refused region would leave the rest of
/// the span claimed in the frame table for good, with no handle left to free it
/// through. The refusal is still reported once the frames are back.
fn reclaim(
    mut arena: Arena<'_>,
    regions: impl IntoIterator<Item = Region>,
) -> Result<(), DmaError> {
    let released = regions.into_iter().try_for_each(|region| arena.release(region));
    arena.reset();
    released
}

impl<A: Arrivals> Device for Block<'_, '_, A> {
    fn sectors(&self) -> u64 {
        self.capacity
    }

    /// Splits `buf` into transfers the data region can hold, one request each.
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        molt_block::bounds(self.capacity, sector, buf)?;
        for (index, chunk) in buf.chunks_mut(TRANSFER).enumerate() {
            let at = sector + (index * TRANSFER / molt_block::SECTOR) as u64;
            let len = u32::try_from(chunk.len()).map_err(|_| BlockError::Range)?;
            let data = Segment::writable(self.data.physical(), len);
            self.command(VIRTIO_BLK_T_IN, at, Some(data))?;
            self.data
                .read_into(0, chunk)
                .map_err(|error| BlockError::from(VirtioError::Dma(error)))?;
        }
        Ok(())
    }
}

impl<A: Arrivals> Disk for Block<'_, '_, A> {
    /// Splits `buf` into transfers the data region can hold, one request each.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        molt_block::bounds(self.capacity, sector, buf)?;
        for (index, chunk) in buf.chunks(TRANSFER).enumerate() {
            let at = sector + (index * TRANSFER / molt_block::SECTOR) as u64;
            self.data
                .write_from(0, chunk)
                .map_err(|error| BlockError::from(VirtioError::Dma(error)))?;
            let len = u32::try_from(chunk.len()).map_err(|_| BlockError::Range)?;
            let data = Segment::readable(self.data.physical(), len);
            self.command(VIRTIO_BLK_T_OUT, at, Some(data))?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.command(VIRTIO_BLK_T_FLUSH, 0, None)?;
        Ok(())
    }
}

/// Reads the device's capacity, in sectors.
///
/// A 64-bit configuration field is two accesses wide, so the read is only
/// coherent if the device's configuration generation did not move across it.
fn capacity(common: &Common<'_>, device: &Mmio<'_>) -> Result<u64, VirtioError> {
    for _ in 0..CONFIG_SPINS {
        let before = common.config_generation()?;
        let low = device.read_u32(CAPACITY_AT)?;
        let high = device.read_u32(CAPACITY_AT + 4)?;
        if common.config_generation()? == before {
            return Ok((high as u64) << 32 | low as u64);
        }
    }
    Err(VirtioError::Device)
}

fn clamp_queue(device_max: u16) -> Result<u16, VirtioError> {
    if device_max == 0 {
        return Err(VirtioError::Device);
    }
    let size = device_max.min(queue::MAX_SIZE);
    if !size.is_power_of_two() {
        return Err(VirtioError::Device);
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use molt_arch::dma::{Arena, DmaError, Region};
    use molt_arch::{FRAME_SIZE, FrameAllocator, MemoryMap, MemoryRegion, MemoryRegionKind};

    use super::{clamp_queue, reclaim};
    use crate::VirtioError;

    struct Map([MemoryRegion; 1]);

    impl MemoryMap for Map {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn region(&self, index: usize) -> Option<MemoryRegion> {
            self.0.get(index).copied()
        }
    }

    #[test]
    fn refused_release_still_frees_span() {
        let map = Map([MemoryRegion::new(
            0x10_0000,
            0x10_0000 + 4 * FRAME_SIZE,
            MemoryRegionKind::Usable,
        )]);
        let mut allocator = FrameAllocator::new(&map);
        let mut slots = [None; 4];
        let mut arena = Arena::claim(&mut allocator, 0, 3, &mut slots).unwrap();
        let held = arena.region(FRAME_SIZE).unwrap();
        let mut elsewhere = [0u8; 16];
        // SAFETY: the array is live for the borrow and nothing else names it.
        let foreign = unsafe { Region::new(elsewhere.as_mut_ptr(), 0x20_0000, 16) };

        let refused = reclaim(arena, [foreign, held]);

        assert_eq!(refused, Err(DmaError::Foreign));
        assert!(slots.iter().all(Option::is_none), "a refused release stranded frames");
    }

    #[test]
    fn deep_device_queue_capped_at_drivers_maximum() {
        let size = clamp_queue(256).expect("a power-of-two queue");

        assert_eq!(size, super::queue::MAX_SIZE, "the driver hosted more than it can");
    }

    #[test]
    fn device_without_queue_refused() {
        assert_eq!(clamp_queue(0), Err(VirtioError::Device));
    }
}
