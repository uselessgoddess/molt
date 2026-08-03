use alloc::boxed::Box;
use core::sync::atomic::{Ordering, fence};

use molt_arch::Mmio;
use molt_arch::dma::Arena;
use molt_arch::iommu::{DeviceId, Mapper, Mapping};
use molt_block::{
    BLOCK, BlockDone, BlockError, BlockOp, Buffer, Device, Disk, Queue as BlockQueue, SECTOR,
};
use molt_core::ring::RequestId;

use crate::{
    Arrivals, CC, Command, DOORBELL, Namespace, NvmeError, Resources, wait_ready, write_command,
};

const FLUSH: u8 = 0x00;
const WRITE: u8 = 0x01;
const READ: u8 = 0x02;

/// The number of independent operations accepted by the I/O queue.
pub const QUEUE_DEPTH: usize = 8;

struct Flight {
    id: RequestId,
    op: Option<BlockOp>,
}

struct Engine<'window, A> {
    registers: Mmio<'window>,
    sq: Mapping,
    cq: Mapping,
    data: Mapping,
    arrivals: A,
    flights: [Option<Flight>; QUEUE_DEPTH],
    ready: [Option<(RequestId, BlockDone)>; QUEUE_DEPTH],
    namespace: Namespace,
    stride: u64,
    tail: u16,
    head: u16,
    phase: bool,
}

impl<A: Arrivals> Engine<'_, A> {
    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        let Some(slot) = self.free_slot() else {
            return Err(op);
        };
        if let Err(error) = validate(self.namespace, &op) {
            self.ready[slot] = Some((id, finish(op, Err(error))));
            return Ok(());
        }

        let data_at = slot as u64 * BLOCK as u64;
        if let BlockOp::Write { bytes, buffer, .. } = &op
            && let Err(error) = self.data.write_from(data_at, &buffer[..*bytes])
        {
            self.ready[slot] = Some((id, finish(op, Err(map_error(error.into())))));
            return Ok(());
        }

        let command = match &op {
            BlockOp::Read { sector, bytes, .. } => {
                self.io_command(READ, slot, *sector, *bytes, self.data.iova().get() + data_at)
            }
            BlockOp::Write { sector, bytes, .. } => {
                self.io_command(WRITE, slot, *sector, *bytes, self.data.iova().get() + data_at)
            }
            BlockOp::Flush => {
                Command { opcode: FLUSH, cid: slot as u16, namespace: 1, ..Command::default() }
            }
        };
        if let Err(error) = write_command(&self.sq, self.tail, command) {
            self.ready[slot] = Some((id, finish(op, Err(map_error(error)))));
            return Ok(());
        }

        self.flights[slot] = Some(Flight { id, op: Some(op) });
        fence(Ordering::Release);
        self.tail = (self.tail + 1) % crate::IO_ENTRIES;
        if let Err(error) =
            self.registers.write_u32(DOORBELL + 2 * self.stride, u32::from(self.tail))
        {
            let flight = self.flights[slot].take().expect("the failed doorbell has one flight");
            self.ready[slot] =
                Some((flight.id, finish(flight.op.unwrap(), Err(map_error(error.into())))));
        }
        Ok(())
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        let mut expired = false;
        loop {
            if let Some(done) = self.ready.iter_mut().find_map(Option::take) {
                return Some(done);
            }
            let at = self.head as u64 * 16;
            let status = match self.cq.read_u16(at + 14) {
                Ok(status) => status,
                Err(_) => return self.fail_one(BlockError::Device),
            };
            if (status & 1 != 0) == self.phase {
                fence(Ordering::Acquire);
                let cid = match self.cq.read_u16(at + 12) {
                    Ok(cid) => cid as usize,
                    Err(_) => return self.fail_one(BlockError::Device),
                };
                if self.advance().is_err() {
                    return self.fail_one(BlockError::Device);
                }
                let Some(flight) = self.flights.get_mut(cid).and_then(Option::take) else {
                    return self.fail_one(BlockError::Device);
                };
                let Some(mut op) = flight.op else {
                    continue;
                };
                let result = if status >> 1 != 0 {
                    Err(BlockError::Device)
                } else {
                    self.copy_read(cid, &mut op)
                };
                return Some((flight.id, finish(op, result)));
            }
            if expired {
                return self.timeout_one();
            }
            if !self.has_live_request() {
                return None;
            }
            expired = self.arrivals.wait() == 0;
        }
    }

    fn io_command(&self, opcode: u8, slot: usize, sector: u64, bytes: usize, data: u64) -> Command {
        let sectors = self.namespace.sectors_per_block();
        let lba = sector / sectors;
        let blocks = bytes / self.namespace.bytes();
        Command::io(opcode, slot as u16, lba, blocks as u16, data)
    }

    fn advance(&mut self) -> Result<(), NvmeError> {
        self.head += 1;
        if self.head == crate::IO_ENTRIES {
            self.head = 0;
            self.phase = !self.phase;
        }
        self.registers
            .write_u32(DOORBELL + 3 * self.stride, u32::from(self.head))
            .map_err(Into::into)
    }

    fn copy_read(&self, slot: usize, op: &mut BlockOp) -> Result<(), BlockError> {
        if let BlockOp::Read { bytes, buffer, .. } = op {
            self.data
                .read_into(slot as u64 * BLOCK as u64, &mut buffer[..*bytes])
                .map_err(|_| BlockError::Device)?;
        }
        Ok(())
    }

    fn free_slot(&self) -> Option<usize> {
        (0..QUEUE_DEPTH).find(|slot| self.flights[*slot].is_none() && self.ready[*slot].is_none())
    }

    fn has_live_request(&self) -> bool {
        self.flights.iter().flatten().any(|flight| flight.op.is_some())
    }

    fn timeout_one(&mut self) -> Option<(RequestId, BlockDone)> {
        let flight = self.flights.iter_mut().flatten().find(|flight| flight.op.is_some())?;
        let op = flight.op.take()?;
        Some((flight.id, finish(op, Err(BlockError::Timeout))))
    }

    fn fail_one(&mut self, error: BlockError) -> Option<(RequestId, BlockDone)> {
        let flight = self.flights.iter_mut().flatten().find(|flight| flight.op.is_some())?;
        let op = flight.op.take()?;
        Some((flight.id, finish(op, Err(error))))
    }
}

pub(crate) struct Parts<'slots, 'window, A, M> {
    pub registers: Mmio<'window>,
    pub endpoint: DeviceId,
    pub arrivals: A,
    pub mapper: M,
    pub arena: Arena<'slots>,
    pub resources: Resources,
    pub namespace: Namespace,
    pub stride: u64,
}

/// One NVM namespace and its multi-request I/O queue.
pub struct Nvme<'slots, 'window, A, M> {
    engine: Engine<'window, A>,
    endpoint: DeviceId,
    mapper: M,
    arena: Arena<'slots>,
    admin_sq: Mapping,
    admin_cq: Mapping,
    identify: Mapping,
    direct: u64,
}

impl<'slots, 'window, A: Arrivals, M: Mapper> Nvme<'slots, 'window, A, M> {
    pub(crate) fn from_parts(parts: Parts<'slots, 'window, A, M>) -> Self {
        let Parts { registers, endpoint, arrivals, mapper, arena, resources, namespace, stride } =
            parts;
        let Resources { admin_sq, admin_cq, identify, io_sq, io_cq, data } = resources;
        let engine = Engine {
            registers,
            sq: io_sq,
            cq: io_cq,
            data,
            arrivals,
            flights: [const { None }; QUEUE_DEPTH],
            ready: [const { None }; QUEUE_DEPTH],
            namespace,
            stride,
            tail: 0,
            head: 0,
            phase: true,
        };
        Self { engine, endpoint, mapper, arena, admin_sq, admin_cq, identify, direct: 0 }
    }

    pub fn capacity(&self) -> u64 {
        self.engine.namespace.sectors()
    }

    pub const fn mapper(&self) -> &M {
        &self.mapper
    }

    pub const fn endpoint(&self) -> DeviceId {
        self.endpoint
    }

    /// Stops the controller, unmaps every page, and returns its mapper.
    pub fn reset(self) -> Result<M, NvmeError> {
        let Self { engine, mut mapper, mut arena, admin_sq, admin_cq, identify, .. } = self;
        let Engine { registers, sq, cq, data, .. } = engine;
        let cap = registers.read_u64(crate::CAP)?;
        registers.write_u32(CC, 0)?;
        wait_ready(&registers, cap, false)?;
        let resources = Resources { admin_sq, admin_cq, identify, io_sq: sq, io_cq: cq, data };
        for mapping in resources.into_mappings() {
            let region = mapper.unmap(mapping).map_err(|error| NvmeError::Dma(error.error()))?;
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

impl<A: Arrivals, M: Mapper> BlockQueue for Nvme<'_, '_, A, M> {
    fn sectors(&self) -> u64 {
        self.capacity()
    }

    fn depth(&self) -> usize {
        QUEUE_DEPTH
    }

    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        self.engine.start(id, op)
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        self.engine.reap()
    }
}

impl<A: Arrivals, M: Mapper> Device for Nvme<'_, '_, A, M> {
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

impl<A: Arrivals, M: Mapper> Disk for Nvme<'_, '_, A, M> {
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

fn validate(namespace: Namespace, op: &BlockOp) -> Result<(), BlockError> {
    match op {
        BlockOp::Read { sector, bytes, buffer } | BlockOp::Write { sector, bytes, buffer } => {
            if *bytes == 0 || *bytes > BLOCK || *bytes > buffer.len() {
                return Err(BlockError::Range);
            }
            if sector % namespace.sectors_per_block() != 0 || bytes % namespace.bytes() != 0 {
                return Err(BlockError::Unaligned);
            }
            molt_block::bounds(namespace.sectors(), *sector, &buffer[..*bytes]).map(|_| ())
        }
        BlockOp::Flush => Ok(()),
    }
}

fn finish(op: BlockOp, result: Result<(), BlockError>) -> BlockDone {
    let buffer = match op {
        BlockOp::Read { buffer, .. } | BlockOp::Write { buffer, .. } => Some(buffer),
        BlockOp::Flush => None,
    };
    BlockDone { result, buffer }
}

fn map_error(error: NvmeError) -> BlockError {
    match error {
        NvmeError::Timeout => BlockError::Timeout,
        NvmeError::Unsupported | NvmeError::Device | NvmeError::Mmio(_) | NvmeError::Dma(_) => {
            BlockError::Device
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use molt_arch::Mmio;
    use molt_arch::dma::Region;
    use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Mapper, Mapping};
    use molt_block::{BLOCK, BlockError, BlockOp};
    use molt_core::ring::RequestId;

    use super::{Engine, QUEUE_DEPTH};
    use crate::{Arrivals, Namespace};

    #[repr(align(4096))]
    struct Aligned<const N: usize>([u8; N]);

    struct NoWait;

    impl Arrivals for NoWait {
        fn wait(&mut self) -> u64 {
            panic!("a published completion must not wait")
        }
    }

    fn mapping(bytes: &mut [u8], physical: u64, perm: DmaPerm) -> Mapping {
        // SAFETY: the array stays live and uniquely models a DMA region.
        let region = unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) };
        Identity.map(DeviceId::new(1), region, perm).ok().unwrap()
    }

    fn engine<'a>(
        registers: &'a mut [u8],
        sq: &mut [u8],
        cq: &mut [u8],
        data: &mut [u8],
        namespace: Namespace,
    ) -> Engine<'a, NoWait> {
        // SAFETY: the register array remains uniquely borrowed by the window.
        let registers = unsafe { Mmio::new(registers.as_mut_ptr(), registers.len() as u64) };
        Engine {
            registers,
            sq: mapping(sq, 0x1000, DmaPerm::READ),
            cq: mapping(cq, 0x2000, DmaPerm::WRITE),
            data: mapping(data, 0x3000, DmaPerm::READ_WRITE),
            arrivals: NoWait,
            flights: [const { None }; QUEUE_DEPTH],
            ready: [const { None }; QUEUE_DEPTH],
            namespace,
            stride: 4,
            tail: 0,
            head: 0,
            phase: true,
        }
    }

    fn read(sector: u64, bytes: usize) -> BlockOp {
        BlockOp::Read { sector, bytes, buffer: Box::new([0; BLOCK]) }
    }

    #[test]
    fn reordered_completions() {
        let mut registers = Aligned([0; 0x2000]);
        let mut sq = Aligned([0; 4096]);
        let mut cq = Aligned([0; 4096]);
        let mut data = Aligned([0; BLOCK * QUEUE_DEPTH]);
        let mut engine = engine(
            &mut registers.0,
            &mut sq.0,
            &mut cq.0,
            &mut data.0,
            Namespace { blocks: 1024, shift: 9 },
        );
        engine.start(RequestId::new(4), read(0, 512)).ok().unwrap();
        engine.start(RequestId::new(5), read(1, 512)).ok().unwrap();
        engine.data.write_u8(0, 4).unwrap();
        engine.data.write_u8(BLOCK as u64, 5).unwrap();
        engine.cq.write_u16(12, 1).unwrap();
        engine.cq.write_u16(14, 1).unwrap();
        engine.cq.write_u16(28, 0).unwrap();
        engine.cq.write_u16(30, 1).unwrap();

        let (later, later_done) = engine.reap().unwrap();
        let (earlier, earlier_done) = engine.reap().unwrap();

        assert_eq!(later, RequestId::new(5));
        assert_eq!(earlier, RequestId::new(4));
        assert_eq!(later_done.buffer.unwrap()[0], 5);
        assert_eq!(earlier_done.buffer.unwrap()[0], 4);
    }

    #[test]
    fn status_failure() {
        let mut registers = Aligned([0; 0x2000]);
        let mut sq = Aligned([0; 4096]);
        let mut cq = Aligned([0; 4096]);
        let mut data = Aligned([0; BLOCK * QUEUE_DEPTH]);
        let mut engine = engine(
            &mut registers.0,
            &mut sq.0,
            &mut cq.0,
            &mut data.0,
            Namespace { blocks: 1024, shift: 9 },
        );
        engine.start(RequestId::new(1), read(0, 512)).ok().unwrap();
        engine.cq.write_u16(12, 0).unwrap();
        engine.cq.write_u16(14, 5).unwrap();

        assert_eq!(engine.reap().unwrap().1.result, Err(BlockError::Device));
    }

    #[test]
    fn partial_lba_rejected() {
        let mut registers = Aligned([0; 0x2000]);
        let mut sq = Aligned([0; 4096]);
        let mut cq = Aligned([0; 4096]);
        let mut data = Aligned([0; BLOCK * QUEUE_DEPTH]);
        let mut engine = engine(
            &mut registers.0,
            &mut sq.0,
            &mut cq.0,
            &mut data.0,
            Namespace { blocks: 32, shift: 12 },
        );

        engine.start(RequestId::new(1), read(0, 512)).ok().unwrap();

        assert_eq!(engine.reap().unwrap().1.result, Err(BlockError::Unaligned));
    }
}
