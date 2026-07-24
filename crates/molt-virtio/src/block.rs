//! The block driver: the handshake that brings a device up, one sector read,
//! and the reset that reclaims its frames.
//!
//! [`Block::start`] runs the modern initialization sequence and programs a
//! single queue out of an [`Arena`] of device-owned frames. [`Block::read`]
//! and [`Block::write`] issue one request and poll the used ring for it, giving
//! up with [`VirtioError::Timeout`] after a bounded spin so a wedged device
//! cannot hang the caller. [`Block::reset`] stops the device *before* it hands
//! the frames back, so no in-flight DMA can land in a reclaimed frame.
//!
//! A write is durable only once [`Block::flush`] returns: the device is free to
//! hold it in a cache until then. The flush is a real `VIRTIO_BLK_T_FLUSH`
//! request when the device offers `VIRTIO_BLK_F_FLUSH`, and a no-op when it does
//! not — a device without a volatile cache needs none.
//!
//! [`molt_block::Device`] and [`molt_block::Write`] are how anything above
//! reaches this: the filesystem reads and writes sectors, not virtqueues, and
//! gets the same contract from a loopback image.

use molt_arch::Mmio;
use molt_arch::dma::{Arena, DmaError, Region};
use molt_block::{BlockError, Device, Write};

use crate::VirtioError;
use crate::config::{Common, status};
use crate::notify::Notify;
use crate::queue::{self, Queue, Segment};
use crate::request::{Completion, Requests};

/// A block read request (`VIRTIO_BLK_T_IN`).
const VIRTIO_BLK_T_IN: u32 = 0;

/// A block write request (`VIRTIO_BLK_T_OUT`).
const VIRTIO_BLK_T_OUT: u32 = 1;

/// A cache flush request (`VIRTIO_BLK_T_FLUSH`).
const VIRTIO_BLK_T_FLUSH: u32 = 4;

/// The feature bit for a device with a flushable write cache
/// (`VIRTIO_BLK_F_FLUSH`, §5.2.3).
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

/// The largest read the driver issues as one request.
const TRANSFER: usize = DATA_BYTES as usize;

/// How long `read` polls the used ring before declaring the request timed out.
const TIMEOUT_SPINS: u32 = 50_000_000;

/// A VirtIO block device driven through one queue of frames it owns.
pub struct Block<'slots, 'w> {
    common: Common<'w>,
    notify: Notify<'w>,
    queue: Queue,
    requests: Requests<{ queue::MAX_SIZE as usize }>,
    control: Region,
    data: Region,
    arena: Arena<'slots>,
    notify_off: u16,
    capacity: u64,
    flushable: bool,
}

impl<'slots, 'w> Block<'slots, 'w> {
    /// Brings a device up over its `common`, `notify`, and `device` windows,
    /// allocating every ring and buffer from `arena`.
    ///
    /// Runs the modern handshake, negotiates `VIRTIO_BLK_F_FLUSH` on top of
    /// `VIRTIO_F_VERSION_1` when the device offers it, and programs queue zero.
    /// A device that offers no usable queue, or rejects the feature set, is
    /// refused rather than left half-initialized.
    pub fn start(
        common: Mmio<'w>,
        notify: Mmio<'w>,
        device: Mmio<'w>,
        notify_multiplier: u32,
        mut arena: Arena<'slots>,
    ) -> Result<Self, VirtioError> {
        let mut common = Common::new(common);
        common.reset()?;
        common.add_status(status::ACKNOWLEDGE)?;
        common.add_status(status::DRIVER)?;
        let accepted = common.negotiate(VIRTIO_BLK_F_FLUSH)?;
        let flushable = accepted & VIRTIO_BLK_F_FLUSH != 0;

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
            control,
            data,
            arena,
            notify_off,
            capacity,
            flushable,
        })
    }

    /// How many sectors the device reports holding.
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Reads `sector` into `buf` in one request; `buf` must fit the data region.
    ///
    /// Submits the three-descriptor read chain — the device writes the data —
    /// and copies the answer out of the data region once it lands.
    fn transfer_in(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), VirtioError> {
        if buf.len() as u64 > self.data.len() {
            return Err(DmaError::Range.into());
        }

        self.header(VIRTIO_BLK_T_IN, sector)?;
        self.run(&[
            Segment::readable(self.control.physical(), HEADER_LEN),
            Segment::writable(self.data.physical(), buf.len() as u32),
            Segment::writable(self.control.physical() + STATUS_AT, 1),
        ])?;
        self.data.read_into(0, buf)?;
        Ok(())
    }

    /// Writes `buf` to `sector` in one request; `buf` must fit the data region.
    ///
    /// The mirror of [`transfer_in`](Self::transfer_in): the data descriptor is
    /// readable this time, so the device takes the bytes rather than filling
    /// them. The write may sit in the device's cache until a [`flush`](Self::flush).
    fn transfer_out(&mut self, sector: u64, buf: &[u8]) -> Result<(), VirtioError> {
        if buf.len() as u64 > self.data.len() {
            return Err(DmaError::Range.into());
        }

        self.data.write_from(0, buf)?;
        self.header(VIRTIO_BLK_T_OUT, sector)?;
        self.run(&[
            Segment::readable(self.control.physical(), HEADER_LEN),
            Segment::readable(self.data.physical(), buf.len() as u32),
            Segment::writable(self.control.physical() + STATUS_AT, 1),
        ])
    }

    /// Empties the device's write cache, if it has one.
    ///
    /// A device that never offered `VIRTIO_BLK_F_FLUSH` has no volatile cache,
    /// so there is nothing to empty and the flush is a no-op. Otherwise it is a
    /// data-less `VIRTIO_BLK_T_FLUSH` request that returns only once every
    /// earlier write is durable.
    fn barrier(&mut self) -> Result<(), VirtioError> {
        if !self.flushable {
            return Ok(());
        }
        self.header(VIRTIO_BLK_T_FLUSH, 0)?;
        self.run(&[
            Segment::readable(self.control.physical(), HEADER_LEN),
            Segment::writable(self.control.physical() + STATUS_AT, 1),
        ])
    }

    /// Lays the request header down and poisons the status byte.
    ///
    /// Poisoning means a device that answers without writing the status is
    /// caught rather than read as success.
    fn header(&mut self, kind: u32, sector: u64) -> Result<(), VirtioError> {
        self.control.write_u32(0, kind)?;
        self.control.write_u32(4, 0)?;
        self.control.write_u64(8, sector)?;
        self.control.write_u8(STATUS_AT, 0xff)?;
        Ok(())
    }

    /// Submits one built request, kicks the device, and polls its completion.
    ///
    /// A device that does not answer within `TIMEOUT_SPINS` has its request
    /// cancelled — the slot stays reserved until the device returns it — and the
    /// call fails with [`VirtioError::Timeout`].
    fn run(&mut self, chain: &[Segment]) -> Result<(), VirtioError> {
        let head = self.queue.push(chain)?;
        let token = self.requests.issue(head);
        self.notify.signal(0, self.notify_off)?;

        for _ in 0..TIMEOUT_SPINS {
            if let Some(used) = self.queue.pop()? {
                if let Completion::Delivered = self.requests.complete(used.head()) {
                    if self.control.read_u8(STATUS_AT)? != VIRTIO_BLK_S_OK {
                        return Err(VirtioError::Device);
                    }
                    return Ok(());
                }
            }
            core::hint::spin_loop();
        }

        self.requests.cancel(token);
        Err(VirtioError::Timeout)
    }

    /// Resets the device and reclaims every frame the arena handed out.
    ///
    /// The reset comes first so the device stops touching the rings and buffers
    /// before the frames behind them return to the table.
    pub fn reset(self) -> Result<(), VirtioError> {
        let Self { mut common, arena, .. } = self;
        common.reset()?;
        arena.reset()?;
        Ok(())
    }
}

impl Device for Block<'_, '_> {
    fn sectors(&self) -> u64 {
        self.capacity
    }

    /// Splits `buf` into transfers the data region can hold, one request each.
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        molt_block::bounds(self.capacity, sector, buf)?;
        for (index, chunk) in buf.chunks_mut(TRANSFER).enumerate() {
            let at = sector + (index * TRANSFER / molt_block::SECTOR) as u64;
            self.transfer_in(at, chunk)?;
        }
        Ok(())
    }
}

impl Write for Block<'_, '_> {
    /// Splits `buf` into transfers the data region can hold, one request each.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        molt_block::bounds(self.capacity, sector, buf)?;
        for (index, chunk) in buf.chunks(TRANSFER).enumerate() {
            let at = sector + (index * TRANSFER / molt_block::SECTOR) as u64;
            self.transfer_out(at, chunk)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.barrier()?;
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
    use super::clamp_queue;
    use crate::VirtioError;

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
