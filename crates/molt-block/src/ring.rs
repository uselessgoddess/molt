//! A ring between whoever wants blocks and whoever holds the device.
//!
//! Calling [`Device::read`] returns when the sectors are there, which is fine
//! for one reader and wrong for everything else: nothing else can be in flight
//! while it waits, so a filesystem that knows it will want the next extent has
//! no way to say so. Submissions here are values with an id, awaited whenever
//! the answer is needed rather than where it was asked for, and the driver at
//! the bottom is the only part still holding a device.
//!
//! Buffers travel with the operation. A submitted read hands its buffer over
//! and gets it back on the completion, so there is nothing for the device to
//! alias and no lifetime for a queue to carry.
//!
//! The depth does not stop here either: the driver holds as many requests at
//! the [`Queue`] as the device takes, so the ring being full is the only thing
//! that ever limits how much is in flight.

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::alloc::AllocError;

use molt_core::ring::{Completion, HeldClient, HeldDriver, IoRing, RequestId, Submission};
use molt_core::task;

use crate::{BlockError, Queue, SECTOR};

/// A block is 4 KiB, the unit every buffer on the ring is sized in.
pub const BLOCK: usize = 4096;

const _: () = assert!(BLOCK % SECTOR == 0);

/// The memory an operation carries down to the device and back.
pub type Buffer = Box<[u8; BLOCK]>;

/// One request on a block ring.
///
/// `bytes` is how much of the buffer the device touches: a whole block for
/// data, one sector for a checkpoint. Addresses stay in sectors, as the
/// [`Device`](crate::Device) below counts them.
pub enum BlockOp {
    /// Fills the first `bytes` of the buffer from `sector` onwards.
    Read { sector: u64, bytes: usize, buffer: Buffer },
    /// Writes the first `bytes` of the buffer at `sector`.
    Write { sector: u64, bytes: usize, buffer: Buffer },
    /// Makes every preceding write durable.
    Flush,
}

/// What the device made of a [`BlockOp`], with the buffer handed back.
pub struct BlockDone {
    pub result: Result<(), BlockError>,
    pub buffer: Option<Buffer>,
}

/// Creates the two ends of a block ring `N` requests deep.
pub fn channel<const N: usize>() -> Result<(BlockClient<N>, BlockDriver<N>), AllocError> {
    let (client, driver) = IoRing::held(Rc::try_new(IoRing::new())?);
    Ok((
        BlockClient { ring: client, parked: Box::try_new([const { None }; N])?, next: 0 },
        BlockDriver { ring: driver, pending: None, held: None, outstanding: 0, barrier: false },
    ))
}

/// The submitting end of a block ring.
pub struct BlockClient<const N: usize> {
    ring: HeldClient<Rc<IoRing<BlockOp, BlockDone, N>>>,
    // moving client should copy pointer rather than `N` answers.
    parked: Box<[Option<Completion<BlockDone>>; N]>,
    next: u64,
}

impl<const N: usize> BlockClient<N> {
    /// Queues `op`, returning it unchanged when the ring is full.
    pub fn submit(&mut self, op: BlockOp) -> Result<RequestId, BlockOp> {
        let id = RequestId::new(self.next);
        match self.ring.try_submit(Submission::new(id, op)) {
            Ok(()) => {
                self.next = self.next.wrapping_add(1);
                Ok(id)
            }
            Err(submission) => Err(submission.into_operation()),
        }
    }

    /// Takes `id`'s answer if it has landed, keeping the ones it passes.
    ///
    /// A device answers in whatever order it finished, and a caller awaiting
    /// the read it needs walks past the readahead it does not. Those are parked
    /// rather than dropped: `N` requests can be in flight and one of them is
    /// `id`, so the rest always fit.
    pub fn take(&mut self, id: RequestId) -> Option<BlockDone> {
        let held =
            |slot: &Option<Completion<BlockDone>>| slot.as_ref().is_some_and(|c| c.id() == id);
        if let Some(at) = self.parked.iter().position(held) {
            return self.parked[at].take().map(Completion::into_result);
        }
        while let Some(completion) = self.ring.try_completion() {
            if completion.id() == id {
                return Some(completion.into_result());
            }
            let free = self.parked.iter_mut().find(|slot| slot.is_none());
            *free.expect("a ring N deep parks at most N - 1 answers") = Some(completion);
        }
        None
    }

    /// Waits for `id`, giving the driver a turn between looks.
    pub async fn settle(&mut self, id: RequestId) -> BlockDone {
        loop {
            if let Some(done) = self.take(id) {
                return done;
            }
            task::defer().await;
        }
    }

    /// Submits `op` and waits for it, letting a full ring drain first.
    pub async fn once(&mut self, mut op: BlockOp) -> BlockDone {
        let id = loop {
            match self.submit(op) {
                Ok(id) => break id,
                Err(refused) => op = refused,
            }
            task::defer().await;
        };
        self.settle(id).await
    }
}

/// The serving end of a block ring.
pub struct BlockDriver<const N: usize> {
    ring: HeldDriver<Rc<IoRing<BlockOp, BlockDone, N>>>,
    /// An answer the completion queue had no room for.
    pending: Option<Completion<BlockDone>>,
    /// A submission the device had no room for, or a flush still waiting for
    /// the writes it makes durable.
    held: Option<(RequestId, BlockOp)>,
    outstanding: usize,
    /// Whether what is at the device is a flush, which runs alone.
    barrier: bool,
}

impl<const N: usize> BlockDriver<N> {
    /// Keeps `queue` as full as its depth allows, returning answers served.
    ///
    /// Neither end may lose what the other handed over: an answer the client
    /// has no room for waits here, and so does a submission the device
    /// refused. A caller that stops taking blocks the driver rather than
    /// dropping what it asked for.
    pub fn pump<Q: Queue>(&mut self, queue: &mut Q) -> usize {
        let mut served = 0;
        loop {
            let drained = self.drain(queue, &mut served);
            if !self.feed(queue) && !drained {
                return served;
            }
        }
    }

    /// Hands finished requests back to the client; false if it moved none.
    fn drain<Q: Queue>(&mut self, queue: &mut Q, served: &mut usize) -> bool {
        let mut moved = false;
        loop {
            if let Some(completion) = self.pending.take() {
                if let Err(refused) = self.ring.try_complete(completion) {
                    self.pending = Some(refused);
                    return moved;
                }
                *served += 1;
                moved = true;
            }
            let Some((id, done)) = queue.reap() else { return moved };
            self.outstanding -= 1;
            if self.outstanding == 0 {
                self.barrier = false;
            }
            self.pending = Some(Completion::new(id, done));
        }
    }

    /// Starts what the device has room for; false if it started none.
    fn feed<Q: Queue>(&mut self, queue: &mut Q) -> bool {
        let mut moved = false;
        while !self.barrier && self.outstanding < queue.depth() {
            let taken = self.held.take().or_else(|| {
                self.ring
                    .try_next()
                    .map(|submission| (submission.id(), submission.into_operation()))
            });
            let Some((id, op)) = taken else { return moved };
            // A flush is the boundary a journal commits across, so it runs on a
            // device with nothing else on it: everything before it has landed,
            // and nothing after it starts until it answers.
            let flush = matches!(op, BlockOp::Flush);
            if flush && self.outstanding > 0 {
                self.held = Some((id, op));
                return moved;
            }
            if let Err(refused) = queue.start(id, op) {
                self.held = Some((id, refused));
                return moved;
            }
            self.outstanding += 1;
            self.barrier = flush;
            moved = true;
        }
        moved
    }
}

/// A driver and the queue it answers from: the bottom of a block ring, where
/// awaiting stops and something has to actually move the sectors.
pub struct Backing<Q, const N: usize> {
    driver: BlockDriver<N>,
    queue: Q,
}

impl<Q: Queue, const N: usize> Backing<Q, N> {
    pub const fn new(driver: BlockDriver<N>, queue: Q) -> Self {
        Self { driver, queue }
    }

    /// Polls `future` to completion, serving the ring between polls.
    pub fn run<F: Future>(&mut self, future: F) -> F::Output {
        let Self { driver, queue } = self;
        task::drive(future, || {
            driver.pump(queue);
        })
    }

    /// The queue underneath, for whoever handed it over.
    pub const fn queue(&mut self) -> &mut Q {
        &mut self.queue
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::{array, iter};

    use super::{BLOCK, Backing, BlockOp, Buffer, channel};
    use crate::{BlockError, Loopback, Queue, Queued, SECTOR, Serial};

    /// Sectors per block, which is what a submission counts in.
    const SECTORS: u64 = (BLOCK / SECTOR) as u64;

    fn buffer() -> Buffer {
        Box::new([0; BLOCK])
    }

    fn read(sector: u64) -> BlockOp {
        BlockOp::Read { sector, bytes: BLOCK, buffer: buffer() }
    }

    /// The image behind a device that takes one request at a time.
    fn serial(bytes: &[u8]) -> Result<Serial<Loopback<'_>>, BlockError> {
        Ok(Serial::new(Loopback::new(bytes)?))
    }

    #[test]
    fn read_lands_through_ring() -> Result<(), BlockError> {
        let bytes = [0xa5; 2 * BLOCK];
        let (mut client, driver) = channel::<4>().unwrap();
        let mut backing = Backing::new(driver, serial(&bytes)?);

        let done = backing.run(client.once(read(0)));

        assert_eq!(done.result, Ok(()));
        assert_eq!(done.buffer.unwrap()[..], bytes[..BLOCK]);
        Ok(())
    }

    #[test]
    fn answers_go_to_who_asked() -> Result<(), BlockError> {
        let mut bytes = [0; 2 * BLOCK];
        bytes[BLOCK] = 7;
        let (mut client, driver) = channel::<4>().unwrap();
        let mut backing = Backing::new(driver, serial(&bytes)?);

        let first = client.submit(read(0)).ok().unwrap();
        let second = client.submit(read((BLOCK / SECTOR) as u64)).ok().unwrap();
        // The later read is awaited first: the earlier answer waits its turn.
        let later = backing.run(client.settle(second));
        let earlier = backing.run(client.settle(first));

        assert_eq!(later.buffer.unwrap()[0], 7);
        assert_eq!(earlier.buffer.unwrap()[0], 0);
        Ok(())
    }

    #[test]
    fn full_ring_waits_for_driver() -> Result<(), BlockError> {
        let bytes = [0; BLOCK];
        let (mut client, driver) = channel::<1>().unwrap();
        let mut backing = Backing::new(driver, serial(&bytes)?);

        client.submit(read(0)).ok().unwrap();
        assert!(client.submit(read(0)).is_err(), "a one deep ring took two");

        assert_eq!(backing.run(client.once(read(0))).result, Ok(()));
        Ok(())
    }

    #[test]
    fn write_reaches_disk_after_flush() -> Result<(), BlockError> {
        let mut bytes = [0; BLOCK];
        let (mut client, driver) = channel::<4>().unwrap();
        {
            let device = Serial::new(Loopback::writable(&mut bytes)?);
            let mut backing = Backing::new(driver, device);
            let mut buffer = buffer();
            buffer[..4].copy_from_slice(b"molt");
            let op = BlockOp::Write { sector: 0, bytes: SECTOR, buffer };
            backing.run(async {
                client.once(op).await.result?;
                client.once(BlockOp::Flush).await.result
            })?;
        }

        assert_eq!(&bytes[..4], b"molt");
        Ok(())
    }

    #[test]
    fn read_past_end_refused() -> Result<(), BlockError> {
        let bytes = [0; BLOCK];
        let (mut client, driver) = channel::<2>().unwrap();
        let mut backing = Backing::new(driver, serial(&bytes)?);

        let done = backing.run(client.once(read(1)));

        assert_eq!(done.result, Err(BlockError::Range));
        Ok(())
    }

    #[test]
    fn every_submission_reaches_deep_device() -> Result<(), BlockError> {
        let bytes = [0; 4 * BLOCK];
        let (mut client, mut driver) = channel::<4>().unwrap();
        let mut queue = Queued::<_, 4>::new(Loopback::new(&bytes)?);

        for block in 0..4 {
            client.submit(read(block * SECTORS)).ok().unwrap();
        }
        driver.feed(&mut queue);

        assert_eq!(iter::from_fn(|| queue.reap()).count(), 4, "the device was left idle");
        Ok(())
    }

    #[test]
    fn reordered_answers_reach_who_asked() -> Result<(), BlockError> {
        let mut bytes = [0; 4 * BLOCK];
        bytes[3 * BLOCK] = 7;
        let (mut client, driver) = channel::<4>().unwrap();
        let mut backing = Backing::new(driver, Queued::<_, 4>::new(Loopback::new(&bytes)?));

        let ids: [_; 4] =
            array::from_fn(|block| client.submit(read(block as u64 * SECTORS)).ok().unwrap());
        // The device answers newest first, so the one awaited here lands last.
        let first = backing.run(client.settle(ids[0]));
        let last = backing.run(client.settle(ids[3]));

        assert_eq!(first.buffer.unwrap()[0], 0);
        assert_eq!(last.buffer.unwrap()[0], 7);
        Ok(())
    }

    #[test]
    fn flush_runs_alone() -> Result<(), BlockError> {
        let mut bytes = [0; 2 * BLOCK];
        let (mut client, mut driver) = channel::<4>().unwrap();
        let mut queue = Queued::<_, 4>::new(Loopback::writable(&mut bytes)?);

        let write = client
            .submit(BlockOp::Write { sector: 0, bytes: SECTOR, buffer: buffer() })
            .ok()
            .unwrap();
        client.submit(BlockOp::Flush).ok().unwrap();
        client.submit(read(0)).ok().unwrap();
        driver.feed(&mut queue);

        assert_eq!(queue.reap().map(|(id, _)| id), Some(write));
        assert!(queue.reap().is_none(), "the flush started beside the write");
        Ok(())
    }
}
