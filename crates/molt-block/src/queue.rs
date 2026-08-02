//! The device as something more than one request can be at.
//!
//! A [`Disk`] answers one call before it hears the next, so a driver over one
//! runs the device at a queue depth of one: the read after this one cannot
//! start until this one landed, and a device that could have been working on
//! both spends the difference idle. [`Queue`] is the same device with a depth
//! of its own — requests it has taken and not answered yet — which is what a
//! virtqueue or an NVMe submission queue offers and what the ring above was
//! built to keep full.
//!
//! Answers come back in whatever order the device finished them, so nothing
//! here preserves submission order; the client above already parks an answer
//! nobody has asked for yet. [`Queued`] answers newest first for that reason: a
//! host device that reorders is the one worth testing against.

use molt_core::ring::RequestId;

use crate::{BlockDone, BlockError, BlockOp, Disk};

/// A device that holds several requests at once.
///
/// Implementors own a request from [`start`](Self::start) until they hand it
/// back through [`reap`](Self::reap) under the id it started with. `sectors` is
/// here because the queue is the only device the ring's users still see.
pub trait Queue {
    /// Sectors the device holds.
    fn sectors(&self) -> u64;

    /// Requests it takes before [`start`](Self::start) refuses, at least one.
    fn depth(&self) -> usize;

    /// Hands `op` to the device, returning it when the queue is full.
    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp>;

    /// Takes one answer the device has finished, if any is.
    fn reap(&mut self) -> Option<(RequestId, BlockDone)>;
}

impl<Q: Queue + ?Sized> Queue for &mut Q {
    fn sectors(&self) -> u64 {
        (**self).sectors()
    }

    fn depth(&self) -> usize {
        (**self).depth()
    }

    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        (**self).start(id, op)
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        (**self).reap()
    }
}

/// A blocking [`Disk`] given a queue `DEPTH` requests deep.
///
/// The disk does the work when a request is reaped rather than when it is
/// started, so requests stay outstanding the way a real device would leave
/// them: everything the driver has handed over is genuinely in flight. The
/// slots are the struct, so nothing allocates once it exists.
pub struct Queued<D, const DEPTH: usize> {
    disk: D,
    waiting: [Option<(RequestId, BlockOp)>; DEPTH],
}

/// A device that takes one request at a time, which is what a [`Disk`] is.
pub type Serial<D> = Queued<D, 1>;

impl<D: Disk, const DEPTH: usize> Queued<D, DEPTH> {
    pub const fn new(disk: D) -> Self {
        const { assert!(DEPTH > 0, "a device that takes nothing never answers") }
        Self { disk, waiting: [const { None }; DEPTH] }
    }

    /// The device underneath, for whoever handed it over.
    pub const fn disk(&mut self) -> &mut D {
        &mut self.disk
    }
}

impl<D: Disk, const DEPTH: usize> Queue for Queued<D, DEPTH> {
    fn sectors(&self) -> u64 {
        self.disk.sectors()
    }

    fn depth(&self) -> usize {
        DEPTH
    }

    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        match self.waiting.iter_mut().find(|slot| slot.is_none()) {
            Some(free) => {
                *free = Some((id, op));
                Ok(())
            }
            None => Err(op),
        }
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        let (id, op) = self.waiting.iter_mut().rev().find_map(Option::take)?;
        Some((id, act(&mut self.disk, op)))
    }
}

fn act<D: Disk>(disk: &mut D, op: BlockOp) -> BlockDone {
    match op {
        BlockOp::Read { sector, bytes, mut buffer } => {
            let result = match buffer.get_mut(..bytes) {
                Some(buf) => disk.read(sector, buf),
                None => Err(BlockError::Range),
            };
            BlockDone { result, buffer: Some(buffer) }
        }
        BlockOp::Write { sector, bytes, buffer } => {
            let result = match buffer.get(..bytes) {
                Some(buf) => disk.write(sector, buf),
                None => Err(BlockError::Range),
            };
            BlockDone { result, buffer: Some(buffer) }
        }
        BlockOp::Flush => BlockDone { result: disk.flush(), buffer: None },
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use molt_core::ring::RequestId;

    use super::{Queue, Queued, Serial};
    use crate::{BLOCK, BlockError, BlockOp, Loopback, SECTOR};

    fn read(sector: u64) -> BlockOp {
        BlockOp::Read { sector, bytes: BLOCK, buffer: Box::new([0; BLOCK]) }
    }

    #[test]
    fn blocking_disk_takes_one() -> Result<(), BlockError> {
        let bytes = [0; BLOCK];
        let mut queue = Serial::new(Loopback::read(&bytes)?);

        assert!(queue.start(RequestId::new(0), read(0)).is_ok());
        assert!(queue.start(RequestId::new(1), read(0)).is_err(), "a one deep device took two");
        Ok(())
    }

    #[test]
    fn deep_device_answers_newest_first() -> Result<(), BlockError> {
        let mut bytes = [0; 2 * BLOCK];
        bytes[BLOCK] = 7;
        let mut queue = Queued::<_, 2>::new(Loopback::read(&bytes)?);

        queue.start(RequestId::new(0), read(0)).ok().unwrap();
        queue.start(RequestId::new(1), read((BLOCK / SECTOR) as u64)).ok().unwrap();

        assert_eq!(queue.reap().unwrap().0, RequestId::new(1));
        assert_eq!(queue.reap().unwrap().0, RequestId::new(0));
        assert!(queue.reap().is_none(), "an empty queue answered");
        Ok(())
    }

    #[test]
    fn reaped_read_carries_bytes() -> Result<(), BlockError> {
        let bytes = [0xa5; BLOCK];
        let mut queue = Serial::new(Loopback::read(&bytes)?);

        queue.start(RequestId::new(0), read(0)).ok().unwrap();
        let (_, done) = queue.reap().unwrap();

        assert_eq!(done.result, Ok(()));
        assert_eq!(done.buffer.unwrap()[..], bytes[..]);
        Ok(())
    }

    #[test]
    fn slot_frees_when_answer_is_taken() -> Result<(), BlockError> {
        let bytes = [0; BLOCK];
        let mut queue = Serial::new(Loopback::read(&bytes)?);

        queue.start(RequestId::new(0), read(0)).ok().unwrap();
        queue.reap().unwrap();

        assert!(queue.start(RequestId::new(1), read(0)).is_ok());
        Ok(())
    }
}
