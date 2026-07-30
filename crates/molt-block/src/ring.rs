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

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::alloc::AllocError;

use molt_core::ring::{Completion, HeldClient, HeldDriver, IoRing, RequestId, Submission};
use molt_core::task::{drive, yielded};

use crate::{BlockError, Disk, SECTOR};

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
        BlockDriver { ring: driver, pending: None },
    ))
}

/// The submitting end of a block ring.
pub struct BlockClient<const N: usize> {
    ring: HeldClient<Rc<IoRing<BlockOp, BlockDone, N>>>,
    /// Boxed: a client is carried inside whatever mounts on it, and moving that
    /// should cost a pointer rather than `N` answers.
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
    /// Completions arrive in submission order, so a caller awaiting the read it
    /// needs walks past the readahead it does not. Those are parked rather than
    /// dropped: `N` requests can be in flight and one of them is `id`, so the
    /// rest always fit.
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
            yielded().await;
        }
    }

    /// Submits `op` and waits for it, letting a full ring drain first.
    pub async fn once(&mut self, mut op: BlockOp) -> BlockDone {
        let id = loop {
            match self.submit(op) {
                Ok(id) => break id,
                Err(refused) => op = refused,
            }
            yielded().await;
        };
        self.settle(id).await
    }
}

/// The serving end of a block ring.
pub struct BlockDriver<const N: usize> {
    ring: HeldDriver<Rc<IoRing<BlockOp, BlockDone, N>>>,
    pending: Option<Completion<BlockDone>>,
}

impl<const N: usize> BlockDriver<N> {
    /// Runs every queued submission against `disk`, returning how many.
    ///
    /// An answer that does not fit is held until the client has taken enough
    /// for it to, which cannot happen while at most `N` are in flight — it is
    /// here so a client that stops taking blocks the driver rather than losing
    /// what it asked for.
    pub fn pump<D: Disk>(&mut self, disk: &mut D) -> usize {
        let mut served = 0;
        loop {
            if let Some(completion) = self.pending.take()
                && let Err(refused) = self.ring.try_complete(completion)
            {
                self.pending = Some(refused);
                return served;
            }
            let Some(submission) = self.ring.try_next() else { return served };
            let (id, op) = (submission.id(), submission.into_operation());
            let done = act(disk, op);
            self.pending = self.ring.try_complete(Completion::new(id, done)).err();
            served += 1;
        }
    }
}

/// A driver and the device it answers from: the bottom of a block ring, where
/// awaiting stops and something has to actually move the sectors.
pub struct Backing<D, const N: usize> {
    driver: BlockDriver<N>,
    device: D,
}

impl<D: Disk, const N: usize> Backing<D, N> {
    pub const fn new(driver: BlockDriver<N>, device: D) -> Self {
        Self { driver, device }
    }

    /// Polls `future` to completion, serving the ring between polls.
    pub fn run<F: Future>(&mut self, future: F) -> F::Output {
        let Self { driver, device } = self;
        drive(future, || {
            driver.pump(device);
        })
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

    use super::{BLOCK, Backing, BlockOp, Buffer, channel};
    use crate::{BlockError, Loopback, SECTOR};

    fn buffer() -> Buffer {
        Box::new([0; BLOCK])
    }

    fn read(sector: u64) -> BlockOp {
        BlockOp::Read { sector, bytes: BLOCK, buffer: buffer() }
    }

    #[test]
    fn read_lands_through_ring() -> Result<(), BlockError> {
        let bytes = [0xa5; 2 * BLOCK];
        let (mut client, driver) = channel::<4>().unwrap();
        let mut backing = Backing::new(driver, Loopback::new(&bytes)?);

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
        let mut backing = Backing::new(driver, Loopback::new(&bytes)?);

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
        let mut backing = Backing::new(driver, Loopback::new(&bytes)?);

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
            let mut backing = Backing::new(driver, Loopback::writable(&mut bytes)?);
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
        let mut backing = Backing::new(driver, Loopback::new(&bytes)?);

        let done = backing.run(client.once(read(1)));

        assert_eq!(done.result, Err(BlockError::Range));
        Ok(())
    }
}
