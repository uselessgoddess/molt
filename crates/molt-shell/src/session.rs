//! One client's side of a filesystem ring.
//!
//! A shell is sequential: it submits an operation, awaits the answer, and only
//! then decides what to ask next. That makes correlation trivial — the single
//! outstanding request is the one every completion belongs to, and a completion
//! carrying another ID means the ring is not this client's alone, which is a
//! protocol error rather than something to skip past.

use core::cell::RefCell;
use core::future::poll_fn;
use core::task::Poll;

use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, Read, ReadWrite, Write};
use molt_core::ring::{IoClient, RequestId, Submission};
use molt_fs::{FsDone, FsError, FsOp};

use crate::ShellError;

/// A ring client and the scratch buffer its reads land in.
///
/// The registry is shared with the filesystem: it writes into the buffer while
/// serving a read, and the shell reads the same bytes out to print them. Both
/// run on one executor and neither holds a borrow across an await, so the
/// [`RefCell`] check never fires — it just makes that rule enforced rather
/// than remembered.
pub struct Session<'ring, 'registry, 'buffer, const R: usize, const N: usize> {
    client: IoClient<'ring, FsOp, Result<FsDone, FsError>, R>,
    buffers: &'registry RefCell<BufferRegistry<'buffer, N>>,
    read: Capability<Read>,
    write: Capability<Write>,
    window: usize,
    next: u64,
}

impl<'ring, 'registry, 'buffer, const R: usize, const N: usize>
    Session<'ring, 'registry, 'buffer, R, N>
{
    /// Talks over `client`, reading into the first `window` bytes of `scratch`.
    ///
    /// `scratch` must already be registered in `buffers`; the two capabilities
    /// this attenuates give the filesystem the right to fill it and the shell
    /// the right to look at what landed, and neither can do the other's half.
    pub fn new(
        client: IoClient<'ring, FsOp, Result<FsDone, FsError>, R>,
        buffers: &'registry RefCell<BufferRegistry<'buffer, N>>,
        scratch: Capability<ReadWrite>,
        window: usize,
    ) -> Result<Self, ShellError> {
        let registry = buffers.borrow();
        let read = registry.read_capability(scratch).map_err(FsError::Handle)?;
        let write = registry.write_capability(scratch).map_err(FsError::Handle)?;
        drop(registry);
        Ok(Self { client, buffers, read, write, window, next: 1 })
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
