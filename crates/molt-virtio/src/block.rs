//! A modern VirtIO block queue with several independent requests in flight.
//!
//! Each slot owns a control record and one 4 KiB bounce buffer inside mapped
//! DMA regions. Submitting only publishes descriptors; completions match the
//! returned descriptor head to the original [`BlockOp`](molt_block::BlockOp),
//! so a device may answer out of order. Flush ordering remains the block
//! scheduler's barrier rather than an accidental consequence of a depth-one
//! driver.

use alloc::boxed::Box;

use molt_arch::Mmio;
use molt_arch::dma::{Arena, DmaError, Region};
use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Mapper, Mapping};
use molt_block::{
    BLOCK, BlockDone, BlockError, BlockOp, Buffer, Device, Disk, Queue as BlockQueue, RequestId,
    SECTOR,
};

use crate::VirtioError;
use crate::config::{Common, status};
use crate::interrupt::Arrivals;
use crate::notify::Notify;
use crate::queue::{self, Queue as Virtqueue};
use crate::request::{Completion, Requests, Token};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_F_ACCESS_PLATFORM: u64 = 1 << 33;

const VIRTIO_BLK_S_OK: u8 = 0;
const CAPACITY_AT: u64 = 0;
const CONFIG_SPINS: u32 = 16;

const HEADER_LEN: u32 = 16;
const CONTROL_STRIDE: u64 = 32;
const STATUS_OFFSET: u64 = HEADER_LEN as u64;

/// The maximum number of block requests this driver owns at once.
pub const REQUESTS: usize = 8;

const CONTROL_BYTES: u64 = CONTROL_STRIDE * REQUESTS as u64;
const DATA_BYTES: u64 = BLOCK as u64 * REQUESTS as u64;

struct Flight {
    id: RequestId,
    head: u16,
    token: Token,
    op: Option<BlockOp>,
}

struct Engine<'window, A> {
    notify: Notify<'window>,
    queue: Virtqueue,
    requests: Requests<{ queue::MAX_SIZE as usize }>,
    arrivals: A,
    control: Mapping,
    data: Mapping,
    flights: [Option<Flight>; REQUESTS],
    ready: [Option<(RequestId, BlockDone)>; REQUESTS],
    notify_off: u16,
    capacity: u64,
    depth: usize,
}

impl<A: Arrivals> Engine<'_, A> {
    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        let Some(slot) = self.free_slot() else {
            return Err(op);
        };
        if let Err(error) = validate(self.capacity, &op) {
            self.ready[slot] = Some((id, finish(op, Err(error))));
            return Ok(());
        }

        let control_at = slot as u64 * CONTROL_STRIDE;
        let data_at = slot as u64 * BLOCK as u64;
        let (kind, sector) = match &op {
            BlockOp::Read { sector, .. } => (VIRTIO_BLK_T_IN, *sector),
            BlockOp::Write { sector, .. } => (VIRTIO_BLK_T_OUT, *sector),
            BlockOp::Flush => (VIRTIO_BLK_T_FLUSH, 0),
        };
        if let BlockOp::Write { bytes, buffer, .. } = &op {
            if let Err(error) = self.data.write_from(data_at, &buffer[..*bytes]) {
                self.ready[slot] = Some((id, finish(op, Err(map_dma(error)))));
                return Ok(());
            }
        }
        if let Err(error) = self.header(control_at, kind, sector) {
            self.ready[slot] = Some((id, finish(op, Err(map_dma(error)))));
            return Ok(());
        }

        let header = match self.control.readable(control_at, HEADER_LEN) {
            Ok(header) => header,
            Err(error) => {
                self.ready[slot] = Some((id, finish(op, Err(map_dma(error)))));
                return Ok(());
            }
        };
        let status = match self.control.writable(control_at + STATUS_OFFSET, 1) {
            Ok(status) => status,
            Err(error) => {
                self.ready[slot] = Some((id, finish(op, Err(map_dma(error)))));
                return Ok(());
            }
        };
        let pushed = match &op {
            BlockOp::Read { bytes, .. } => self
                .data
                .writable(data_at, *bytes as u32)
                .map_err(VirtioError::from)
                .and_then(|data| self.queue.push(&[header, data, status])),
            BlockOp::Write { bytes, .. } => self
                .data
                .readable(data_at, *bytes as u32)
                .map_err(VirtioError::from)
                .and_then(|data| self.queue.push(&[header, data, status])),
            BlockOp::Flush => self.queue.push(&[header, status]),
        };
        let head = match pushed {
            Ok(head) => head,
            Err(VirtioError::Full) => return Err(op),
            Err(error) => {
                self.ready[slot] = Some((id, finish(op, Err(error.into()))));
                return Ok(());
            }
        };

        let token = self.requests.issue(head);
        self.flights[slot] = Some(Flight { id, head, token, op: Some(op) });
        if let Err(error) = self.notify.signal(0, self.notify_off) {
            let flight = self.flights[slot].as_mut().expect("the published request has a slot");
            self.requests.cancel(flight.token);
            let op = flight.op.take().expect("a fresh request still owns its operation");
            self.ready[slot] = Some((id, finish(op, Err(error.into()))));
        }
        Ok(())
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        loop {
            if let Some(done) = self.ready.iter_mut().find_map(Option::take) {
                return Some(done);
            }
            match self.queue.pop() {
                Ok(Some(used)) => {
                    let Some(slot) = self.flights.iter().position(|flight| {
                        flight.as_ref().is_some_and(|flight| flight.head == used.head())
                    }) else {
                        return self.fail_one(BlockError::Device);
                    };
                    let flight = self.flights[slot].take().expect("the matching flight exists");
                    if self.requests.complete(used.head()) == Completion::Stale {
                        continue;
                    }
                    let Some(op) = flight.op else {
                        continue;
                    };
                    return Some((flight.id, self.complete_with_copy(slot, op)));
                }
                Ok(None) if self.has_live_request() => {
                    if self.arrivals.wait() == 0 {
                        return self.timeout_one();
                    }
                }
                Ok(None) => return None,
                Err(_) => return self.fail_one(BlockError::Device),
            }
        }
    }

    fn header(&self, offset: u64, kind: u32, sector: u64) -> Result<(), DmaError> {
        self.control.write_u32(offset, kind)?;
        self.control.write_u32(offset + 4, 0)?;
        self.control.write_u64(offset + 8, sector)?;
        self.control.write_u8(offset + STATUS_OFFSET, 0xff)
    }

    fn complete(&self, slot: usize, op: &BlockOp) -> Result<(), BlockError> {
        let control_at = slot as u64 * CONTROL_STRIDE;
        if self.control.read_u8(control_at + STATUS_OFFSET).map_err(map_dma)? != VIRTIO_BLK_S_OK {
            return Err(BlockError::Device);
        }
        if let BlockOp::Read { bytes, .. } = op {
            if *bytes > BLOCK {
                return Err(BlockError::Range);
            }
        }
        Ok(())
    }

    fn copy_read(&self, slot: usize, op: &mut BlockOp) -> Result<(), BlockError> {
        if let BlockOp::Read { bytes, buffer, .. } = op {
            self.data
                .read_into(slot as u64 * BLOCK as u64, &mut buffer[..*bytes])
                .map_err(map_dma)?;
        }
        Ok(())
    }

    fn free_slot(&self) -> Option<usize> {
        (0..self.depth).find(|slot| self.flights[*slot].is_none() && self.ready[*slot].is_none())
    }

    fn has_live_request(&self) -> bool {
        self.flights.iter().flatten().any(|flight| flight.op.is_some())
    }

    fn timeout_one(&mut self) -> Option<(RequestId, BlockDone)> {
        let flight = self.flights.iter_mut().flatten().find(|flight| flight.op.is_some())?;
        self.requests.cancel(flight.token);
        let op = flight.op.take()?;
        Some((flight.id, finish(op, Err(BlockError::Timeout))))
    }

    fn fail_one(&mut self, error: BlockError) -> Option<(RequestId, BlockDone)> {
        let flight = self.flights.iter_mut().flatten().find(|flight| flight.op.is_some())?;
        self.requests.cancel(flight.token);
        let op = flight.op.take()?;
        Some((flight.id, finish(op, Err(error))))
    }

    fn complete_with_copy(&self, slot: usize, mut op: BlockOp) -> BlockDone {
        let result = self.complete(slot, &op).and_then(|()| self.copy_read(slot, &mut op));
        finish(op, result)
    }

    fn mappings(self) -> impl Iterator<Item = Mapping> {
        self.queue.mappings().into_iter().chain([self.control, self.data])
    }
}

/// A VirtIO block device driven through one translated queue.
pub struct Block<'slots, 'window, A, M = Identity> {
    common: Common<'window>,
    engine: Engine<'window, A>,
    mapper: M,
    arena: Arena<'slots>,
    direct: u64,
}

impl<'slots, 'window, A: Arrivals> Block<'slots, 'window, A, Identity> {
    /// Starts a block device that addresses physical memory directly.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        common: Mmio<'window>,
        notify: Mmio<'window>,
        device: Mmio<'window>,
        notify_multiplier: u32,
        vector: u16,
        arrivals: A,
        endpoint: DeviceId,
        arena: Arena<'slots>,
    ) -> Result<Self, VirtioError> {
        Self::start_mapped(
            common,
            notify,
            device,
            notify_multiplier,
            vector,
            arrivals,
            endpoint,
            arena,
            Identity,
        )
    }
}

impl<'slots, 'window, A: Arrivals, M: Mapper> Block<'slots, 'window, A, M> {
    /// Starts a block device in `endpoint`'s DMA address space.
    #[allow(clippy::too_many_arguments)]
    pub fn start_mapped(
        common: Mmio<'window>,
        notify: Mmio<'window>,
        device: Mmio<'window>,
        notify_multiplier: u32,
        vector: u16,
        arrivals: A,
        endpoint: DeviceId,
        mut arena: Arena<'slots>,
        mut mapper: M,
    ) -> Result<Self, VirtioError> {
        let mut common = Common::new(common);
        common.reset()?;
        common.add_status(status::ACKNOWLEDGE)?;
        common.add_status(status::DRIVER)?;
        let platform = mapper.access_platform();
        let wanted = VIRTIO_BLK_F_RO
            | VIRTIO_BLK_F_FLUSH
            | if platform { VIRTIO_F_ACCESS_PLATFORM } else { 0 };
        let features = common.negotiate(wanted)?;
        if features & VIRTIO_BLK_F_RO != 0 {
            return Err(VirtioError::ReadOnly);
        }
        if features & VIRTIO_BLK_F_FLUSH == 0
            || platform && features & VIRTIO_F_ACCESS_PLATFORM == 0
        {
            return Err(VirtioError::Features);
        }
        let capacity = capacity(&common, &device)?;

        common.select_queue(0)?;
        let size = clamp_queue(common.queue_size()?)?;
        let depth = (size as usize / 3).min(REQUESTS);
        if depth < 2 {
            return Err(VirtioError::Device);
        }

        let descriptors = mapped(
            &mut mapper,
            endpoint,
            arena.region(queue::descriptor_bytes(size))?,
            DmaPerm::READ,
        )?;
        let driver =
            mapped(&mut mapper, endpoint, arena.region(queue::driver_bytes(size))?, DmaPerm::READ)?;
        let device_ring = mapped(
            &mut mapper,
            endpoint,
            arena.region(queue::device_bytes(size))?,
            DmaPerm::WRITE,
        )?;
        let control =
            mapped(&mut mapper, endpoint, arena.region(CONTROL_BYTES)?, DmaPerm::READ_WRITE)?;
        let data = mapped(&mut mapper, endpoint, arena.region(DATA_BYTES)?, DmaPerm::READ_WRITE)?;

        let queue = Virtqueue::new(size, descriptors, driver, device_ring)?;
        common.set_queue_size(size)?;
        common.set_queue_vector(vector)?;
        common.set_queue_rings(
            queue.descriptors_iova(),
            queue.driver_iova(),
            queue.device_iova(),
        )?;
        common.enable_queue()?;
        let notify_off = common.queue_notify_off()?;
        common.add_status(status::DRIVER_OK)?;

        let engine = Engine {
            notify: Notify::new(notify, notify_multiplier),
            queue,
            requests: Requests::new(),
            arrivals,
            control,
            data,
            flights: [const { None }; REQUESTS],
            ready: [const { None }; REQUESTS],
            notify_off,
            capacity,
            depth,
        };
        Ok(Self { common, engine, mapper, arena, direct: 0 })
    }

    pub const fn capacity(&self) -> u64 {
        self.engine.capacity
    }

    /// The translation backend that owns this device's DMA address space.
    pub const fn mapper(&self) -> &M {
        &self.mapper
    }

    /// Stops the device, removes every mapping, and reclaims its arena.
    pub fn reset(self) -> Result<M, VirtioError> {
        let Self { mut common, engine, mut mapper, mut arena, .. } = self;
        common.reset()?;
        for mapping in engine.mappings() {
            let region = mapper.unmap(mapping).map_err(|error| VirtioError::Dma(error.error()))?;
            arena.release(region)?;
        }
        arena.reset();
        Ok(mapper)
    }

    fn execute(&mut self, mut op: BlockOp) -> BlockDone {
        let id = RequestId::new(self.direct);
        self.direct = self.direct.wrapping_add(1);
        loop {
            match self.engine.start(id, op) {
                Ok(()) => break,
                Err(refused) => op = refused,
            }
            if let Some((_, done)) = self.engine.reap() {
                return done;
            }
        }
        loop {
            if let Some((answered, done)) = self.engine.reap() {
                debug_assert_eq!(answered, id);
                return done;
            }
        }
    }
}

impl<A: Arrivals, M: Mapper> BlockQueue for Block<'_, '_, A, M> {
    fn sectors(&self) -> u64 {
        self.engine.capacity
    }

    fn depth(&self) -> usize {
        self.engine.depth
    }

    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        self.engine.start(id, op)
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        self.engine.reap()
    }
}

impl<A: Arrivals, M: Mapper> Device for Block<'_, '_, A, M> {
    fn sectors(&self) -> u64 {
        self.capacity()
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        molt_block::bounds(self.capacity(), sector, buf)?;
        for (index, chunk) in buf.chunks_mut(BLOCK).enumerate() {
            let at = sector + (index * BLOCK / SECTOR) as u64;
            let op = BlockOp::Read { sector: at, bytes: chunk.len(), buffer: Box::new([0; BLOCK]) };
            let done = self.execute(op);
            done.result?;
            let buffer = done.buffer.ok_or(BlockError::Device)?;
            chunk.copy_from_slice(&buffer[..chunk.len()]);
        }
        Ok(())
    }
}

impl<A: Arrivals, M: Mapper> Disk for Block<'_, '_, A, M> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        molt_block::bounds(self.capacity(), sector, buf)?;
        for (index, chunk) in buf.chunks(BLOCK).enumerate() {
            let at = sector + (index * BLOCK / SECTOR) as u64;
            let mut buffer: Buffer = Box::new([0; BLOCK]);
            buffer[..chunk.len()].copy_from_slice(chunk);
            self.execute(BlockOp::Write { sector: at, bytes: chunk.len(), buffer }).result?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.execute(BlockOp::Flush).result
    }
}

fn finish(op: BlockOp, result: Result<(), BlockError>) -> BlockDone {
    let buffer = match op {
        BlockOp::Read { buffer, .. } | BlockOp::Write { buffer, .. } => Some(buffer),
        BlockOp::Flush => None,
    };
    BlockDone { result, buffer }
}

fn validate(capacity: u64, op: &BlockOp) -> Result<(), BlockError> {
    match op {
        BlockOp::Read { sector, bytes, buffer } | BlockOp::Write { sector, bytes, buffer } => {
            if *bytes == 0 || *bytes > BLOCK || *bytes > buffer.len() {
                return Err(BlockError::Range);
            }
            molt_block::bounds(capacity, *sector, &buffer[..*bytes]).map(|_| ())
        }
        BlockOp::Flush => Ok(()),
    }
}

fn map_dma(error: DmaError) -> BlockError {
    BlockError::from(VirtioError::Dma(error))
}

fn mapped(
    mapper: &mut impl Mapper,
    endpoint: DeviceId,
    region: Region,
    perm: DmaPerm,
) -> Result<Mapping, VirtioError> {
    mapper.map(endpoint, region, perm).map_err(|error| VirtioError::Dma(error.error()))
}

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
    use alloc::boxed::Box;

    use molt_arch::Mmio;
    use molt_arch::dma::Region;
    use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Mapper, Mapping};
    use molt_block::{BLOCK, BlockError, BlockOp, RequestId};

    use super::{CONTROL_BYTES, Engine, REQUESTS, clamp_queue};
    use crate::VirtioError;
    use crate::notify::Notify;
    use crate::queue::{self, Queue};
    use crate::request::Requests;

    struct NoWait;

    impl crate::Arrivals for NoWait {
        fn wait(&mut self) -> u64 {
            panic!("a published used entry must not wait")
        }
    }

    fn region(bytes: &mut [u8], physical: u64) -> Region {
        // SAFETY: the arrays remain live and stand in for device-owned bytes.
        unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) }
    }

    fn mapping(bytes: &mut [u8], physical: u64, perm: DmaPerm) -> Mapping {
        Identity.map(DeviceId::new(1), region(bytes, physical), perm).ok().unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn engine<'a>(
        descriptors: &mut [u8],
        driver: &mut [u8],
        device: &mut [u8],
        control: &mut [u8],
        data: &mut [u8],
        notify: &'a mut [u8],
    ) -> Engine<'a, NoWait> {
        let size = queue::MAX_SIZE;
        let queue = Queue::new(
            size,
            mapping(descriptors, 0x1000, DmaPerm::READ),
            mapping(driver, 0x2000, DmaPerm::READ),
            mapping(device, 0x3000, DmaPerm::WRITE),
        )
        .unwrap();
        // SAFETY: the notification array remains live and uniquely represents
        // this fake BAR window.
        let notify = unsafe { Mmio::new(notify.as_mut_ptr(), notify.len() as u64) };
        Engine {
            notify: Notify::new(notify, 0),
            queue,
            requests: Requests::new(),
            arrivals: NoWait,
            control: mapping(control, 0x4000, DmaPerm::READ_WRITE),
            data: mapping(data, 0x8000, DmaPerm::READ_WRITE),
            flights: [const { None }; REQUESTS],
            ready: [const { None }; REQUESTS],
            notify_off: 0,
            capacity: 1024,
            depth: REQUESTS,
        }
    }

    fn read(sector: u64) -> BlockOp {
        BlockOp::Read { sector, bytes: BLOCK, buffer: Box::new([0; BLOCK]) }
    }

    #[test]
    fn two_requests_publish_before_completion() -> Result<(), VirtioError> {
        let mut descriptors = [0u8; queue::MAX_SIZE as usize * 16];
        let mut driver = [0u8; 72];
        let mut device = [0u8; 264];
        let mut control = [0u8; CONTROL_BYTES as usize];
        let mut data = [0u8; BLOCK * REQUESTS];
        let mut notify = [0u8; 2];
        let mut engine = engine(
            &mut descriptors,
            &mut driver,
            &mut device,
            &mut control,
            &mut data,
            &mut notify,
        );

        engine.start(RequestId::new(4), read(0)).ok().unwrap();
        engine.start(RequestId::new(5), read(8)).ok().unwrap();

        assert_eq!(u16::from_le_bytes(driver[2..4].try_into().unwrap()), 2);
        assert_eq!(engine.queue.available(), queue::MAX_SIZE - 6);
        Ok(())
    }

    #[test]
    fn out_of_order_reads_keep_their_buffers() -> Result<(), VirtioError> {
        let mut descriptors = [0u8; queue::MAX_SIZE as usize * 16];
        let mut driver = [0u8; 72];
        let mut device = [0u8; 264];
        let mut control = [0u8; CONTROL_BYTES as usize];
        let mut data = [0u8; BLOCK * REQUESTS];
        let mut notify = [0u8; 2];
        let mut engine = engine(
            &mut descriptors,
            &mut driver,
            &mut device,
            &mut control,
            &mut data,
            &mut notify,
        );
        engine.start(RequestId::new(4), read(0)).ok().unwrap();
        engine.start(RequestId::new(5), read(8)).ok().unwrap();
        let first = u16::from_le_bytes(driver[4..6].try_into().unwrap());
        let second = u16::from_le_bytes(driver[6..8].try_into().unwrap());
        engine.control.write_u8(16, 0)?;
        engine.control.write_u8(32 + 16, 0)?;
        engine.data.write_u8(0, 4)?;
        engine.data.write_u8(BLOCK as u64, 5)?;
        device[4..8].copy_from_slice(&(second as u32).to_le_bytes());
        device[12..16].copy_from_slice(&(first as u32).to_le_bytes());
        device[2..4].copy_from_slice(&2u16.to_le_bytes());

        let (later, later_done) = engine.reap().unwrap();
        let (earlier, earlier_done) = engine.reap().unwrap();

        assert_eq!(later, RequestId::new(5));
        assert_eq!(earlier, RequestId::new(4));
        assert_eq!(later_done.result, Ok(()));
        assert_eq!(earlier_done.result, Ok(()));
        assert_eq!(later_done.buffer.unwrap()[0], 5);
        assert_eq!(earlier_done.buffer.unwrap()[0], 4);
        Ok(())
    }

    #[test]
    fn device_status_reaches_block_error() -> Result<(), VirtioError> {
        let mut descriptors = [0u8; queue::MAX_SIZE as usize * 16];
        let mut driver = [0u8; 72];
        let mut device = [0u8; 264];
        let mut control = [0u8; CONTROL_BYTES as usize];
        let mut data = [0u8; BLOCK * REQUESTS];
        let mut notify = [0u8; 2];
        let mut engine = engine(
            &mut descriptors,
            &mut driver,
            &mut device,
            &mut control,
            &mut data,
            &mut notify,
        );
        engine.start(RequestId::new(1), read(0)).ok().unwrap();
        let head = u16::from_le_bytes(driver[4..6].try_into().unwrap());
        engine.control.write_u8(16, 1)?;
        device[4..8].copy_from_slice(&(head as u32).to_le_bytes());
        device[2..4].copy_from_slice(&1u16.to_le_bytes());

        assert_eq!(engine.reap().unwrap().1.result, Err(BlockError::Device));
        Ok(())
    }

    #[test]
    fn deep_device_queue_capped_at_drivers_maximum() -> Result<(), VirtioError> {
        assert_eq!(clamp_queue(256)?, queue::MAX_SIZE);
        Ok(())
    }

    #[test]
    fn device_without_queue_refused() {
        assert_eq!(clamp_queue(0), Err(VirtioError::Device));
    }
}
