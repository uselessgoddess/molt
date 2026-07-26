//! One client's side of a filesystem ring.
//!
//! A shell is sequential: it submits an operation, awaits the answer, and only
//! then decides what to ask next. That makes correlation almost trivial — the
//! single outstanding request is the one nearly every completion belongs to.
//! The exception is what a restart leaves behind: an epoch that ended answers
//! the requests it took off the ring, and those answers can arrive after the
//! client asking has itself been restarted. An ID older than the one awaited is
//! that; an ID newer than it means the ring is not this client's alone, which
//! is a protocol error.

use core::cell::RefCell;
use core::future::poll_fn;
use core::task::Poll;

use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, Read, ReadWrite, Write};
use molt_core::registry::Registry;
use molt_core::ring::{IoClient, RequestId, Submission};
use molt_fs::{Dir, FsDone, FsError, FsOp, Storage};

use crate::ShellError;

/// A ring client, the namespace it finds storage in, and its scratch buffer.
///
/// The buffer registry is shared with the filesystem: it writes into the buffer
/// while serving a read, and the shell reads the same bytes out to print them.
/// Both run on one executor and neither holds a borrow across an await, so the
/// [`RefCell`] check never fires — it just makes that rule enforced rather
/// than remembered. The name registry is shared for the same reason and one
/// more: the service writes to it exactly when it publishes or withdraws, which
/// are the two moments a client must not be holding it.
pub struct Session<'ring, 'registry, 'buffer, const R: usize, const N: usize, const M: usize> {
    client: IoClient<'ring, FsOp, Result<FsDone, FsError>, R>,
    buffers: &'registry RefCell<BufferRegistry<'buffer, N>>,
    names: &'registry RefCell<Registry<Storage, M>>,
    read: Capability<Read>,
    write: Capability<Write>,
    window: usize,
    next: u64,
    lease: Option<Capability<Storage>>,
}

impl<'ring, 'registry, 'buffer, const R: usize, const N: usize, const M: usize>
    Session<'ring, 'registry, 'buffer, R, N, M>
{
    /// Talks over `client`, reading into the first `window` bytes of `scratch`.
    ///
    /// `scratch` must already be registered in `buffers`; the two capabilities
    /// this attenuates give the filesystem the right to fill it and the shell
    /// the right to look at what landed, and neither can do the other's half.
    /// No lease is taken here: a session that starts before anything publishes
    /// storage is ordinary, and the first command is where that shows.
    pub fn new(
        client: IoClient<'ring, FsOp, Result<FsDone, FsError>, R>,
        buffers: &'registry RefCell<BufferRegistry<'buffer, N>>,
        names: &'registry RefCell<Registry<Storage, M>>,
        scratch: Capability<ReadWrite>,
        window: usize,
    ) -> Result<Self, ShellError> {
        let registry = buffers.borrow();
        let read = registry.read_capability(scratch).map_err(FsError::Handle)?;
        let write = registry.write_capability(scratch).map_err(FsError::Handle)?;
        drop(registry);
        Ok(Self { client, buffers, names, read, write, window, next: 1, lease: None })
    }

    /// The root directory of whatever answers for storage now.
    ///
    /// A lease is kept between commands and re-acquired when it stops reading,
    /// so a client that was talking to a filesystem which restarted finds the
    /// new one by asking the same question again rather than by being handed
    /// anything. Nothing published means no storage, not a failure to recover.
    pub fn root(&mut self) -> Result<Capability<Dir>, ShellError> {
        if let Some(root) = self.leased() {
            return Ok(root);
        }
        self.lease = None;
        let acquired = self.names.borrow().acquire();
        self.lease = Some(acquired.map_err(|_| ShellError::Unavailable)?);
        self.leased().ok_or(ShellError::Unavailable)
    }

    /// Gives up the lease, so the next command acquires a current one.
    pub fn release(&mut self) {
        self.lease = None;
    }

    /// Forgets the epoch this session was in the middle of.
    ///
    /// Whatever the ring still holds for it was answered by somebody else's
    /// restart, and the handles behind those answers are gone; keeping the
    /// completions would only mean matching them against requests no one will
    /// ever submit again.
    pub fn reset(&mut self) {
        self.release();
        while self.client.try_completion().is_some() {}
    }

    /// The root the lease in hand reads, if it still reads one.
    fn leased(&self) -> Option<Capability<Dir>> {
        let lease = self.lease?;
        Some(self.names.borrow().endpoint(lease).ok()?.root())
    }

    /// How many bytes one read can bring back.
    pub const fn window(&self) -> usize {
        self.window
    }

    /// The buffer a read fills, as the filesystem names it.
    pub const fn target(&self) -> BufferOperation<Write> {
        BufferOperation::new(self.write, 0, self.window)
    }

    /// Submits `op` and waits for its completion.
    ///
    /// Nothing wakes this task when the answer arrives: the filesystem driver
    /// runs on the same executor and posts completions without a waker, so a
    /// poll that finds the queue empty asks to be polled again rather than
    /// pretending an interrupt is coming.
    pub async fn request(&mut self, op: FsOp) -> Result<FsDone, ShellError> {
        let id = RequestId::new(self.next);
        self.next = self.next.wrapping_add(1);

        let client = &mut self.client;
        let mut waiting = Some(Submission::new(id, op));
        poll_fn(move |context| {
            if let Some(submission) = waiting.take()
                && let Err(refused) = client.try_submit(submission)
            {
                waiting = Some(refused);
                context.waker().wake_by_ref();
                return Poll::Pending;
            }
            match client.try_completion() {
                Some(completion) if completion.id() == id => {
                    Poll::Ready(completion.into_result().map_err(ShellError::Fs))
                }
                // The answer to something an ended epoch cancelled, arriving
                // after whoever was waiting for it stopped waiting.
                Some(completion) if completion.id() < id => {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
                Some(_) => Poll::Ready(Err(ShellError::Protocol)),
                None => {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        })
        .await
    }

    /// Hands `bytes` of what a read brought back to `use_bytes`.
    ///
    /// The borrow lives only as long as the call, which is why this takes a
    /// closure instead of returning the slice.
    pub fn taken<T>(
        &self,
        bytes: usize,
        use_bytes: impl FnOnce(&[u8]) -> T,
    ) -> Result<T, ShellError> {
        let registry = self.buffers.borrow();
        let operation = BufferOperation::new(self.read, 0, bytes);
        let taken = registry.resolve_read(operation).map_err(FsError::Buffer)?;
        Ok(use_bytes(taken))
    }
}
