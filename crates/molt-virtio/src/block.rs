//! The block driver: the handshake that brings a device up, one sector read,
//! and the reset that reclaims its frames.
//!
//! [`Block::start`] runs the modern initialization sequence and programs a
//! single queue out of an [`Arena`] of device-owned frames. [`Block::read`]
//! issues one read request and polls the used ring for it, giving up with
//! [`VirtioError::Timeout`] after a bounded spin so a wedged device cannot hang
//! the caller. [`Block::reset`] stops the device *before* it hands the frames
//! back, so no in-flight DMA can land in a reclaimed frame.
//!
//! The write path is absent by design: Stage 2.4's filesystem is read-only, so
//! the driver never marks a sector writable to the device or issues a flush.

use molt_arch::Mmio;
use molt_arch::dma::{Arena, DmaError, Region};

use crate::VirtioError;
use crate::config::{Common, status};
use crate::notify::Notify;
use crate::queue::{self, Queue, Segment};
use crate::request::{Completion, Requests};

/// A block read request (`VIRTIO_BLK_T_IN`).
const VIRTIO_BLK_T_IN: u32 = 0;

/// The status byte a device writes on success (`VIRTIO_BLK_S_OK`).
const VIRTIO_BLK_S_OK: u8 = 0;

/// A sector is 512 bytes, the unit `read` addresses in.
pub const SECTOR: usize = 512;

/// The request header the device reads: type, reserved, sector.
const HEADER_LEN: u32 = 16;

/// Where the one-byte status sits in the control region, past the header.
const STATUS_AT: u64 = HEADER_LEN as u64;

/// The control region holds the header and the trailing status byte.
const CONTROL_BYTES: u64 = HEADER_LEN as u64 + 1;

/// The data region is one frame, enough for any single-sector read.
const DATA_BYTES: u64 = 4096;

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
}

impl<'slots, 'w> Block<'slots, 'w> {
    /// Brings a device up over its `common` and `notify` windows, allocating
    /// every ring and buffer from `arena`.
    ///
    /// Runs the modern handshake, negotiates only `VIRTIO_F_VERSION_1` (the
    /// driver needs no block feature to read), and programs queue zero. A device
    /// that offers no usable queue, or rejects the feature set, is refused
    /// rather than left half-initialized.
    pub fn start(
        common: Mmio<'w>,
        notify: Mmio<'w>,
        notify_multiplier: u32,
        mut arena: Arena<'slots>,
    ) -> Result<Self, VirtioError> {
        let mut common = Common::new(common);
        common.reset()?;
        common.add_status(status::ACKNOWLEDGE)?;
        common.add_status(status::DRIVER)?;
        common.negotiate(0)?;

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
        })
    }

    /// Reads `sector` into `buf`, which must fit in the data region.
    ///
    /// Submits the three-descriptor read chain, kicks the device, and polls its
    /// completion. A device that does not answer within `TIMEOUT_SPINS` has
    /// its request cancelled — the slot stays reserved until the device returns
    /// it — and the read fails with [`VirtioError::Timeout`].
    pub fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), VirtioError> {
        if buf.len() as u64 > self.data.len() {
            return Err(DmaError::Range.into());
        }

        self.control.write_u32(0, VIRTIO_BLK_T_IN)?;
        self.control.write_u32(4, 0)?;
        self.control.write_u64(8, sector)?;
        // Poison the status so a device that answers without writing it is
        // caught rather than read as success.
        self.control.write_u8(STATUS_AT, 0xff)?;

        let head = self.queue.push(&[
            Segment::readable(self.control.physical(), HEADER_LEN),
            Segment::writable(self.data.physical(), buf.len() as u32),
            Segment::writable(self.control.physical() + STATUS_AT, 1),
        ])?;
        let token = self.requests.issue(head);
        self.notify.signal(0, self.notify_off)?;

        for _ in 0..TIMEOUT_SPINS {
            if let Some(used) = self.queue.pop()? {
                if let Completion::Delivered = self.requests.complete(used.head()) {
                    if self.control.read_u8(STATUS_AT)? != VIRTIO_BLK_S_OK {
                        return Err(VirtioError::Device);
                    }
                    self.data.read_into(0, buf)?;
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
