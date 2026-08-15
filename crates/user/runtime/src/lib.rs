#![no_std]

//! Safe program-side handles, ring requests, futures, and heap support.
//!
//! Programs use this crate instead of constructing `molt-abi` wire values.
//! There is one outstanding request per [`Client`], which makes completion
//! correlation a property of the mutable borrow held by [`Request`].

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use molt_abi::{Call, Channel, Domain, Op, Region};

const KICK: u64 = 0;
const EXIT: u64 = 1;

/// A typed capability supplied by the kernel.
#[derive(Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct Handle<T> {
    raw: molt_abi::Handle,
    kind: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    /// Wraps a handle the trusted bootstrap passed to the domain.
    ///
    /// The value is still checked by the kernel on every operation; this
    /// constructor does not turn it into authority by itself.
    pub const fn from_raw(raw: u64) -> Self {
        Self { raw: molt_abi::Handle::new(raw), kind: PhantomData }
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

/// A byte-output capability.
pub enum Output {}

/// A timer capability (the current ABI carries its duration in the request).
pub enum Timer {}

/// A directory capability.
pub enum Directory {}

/// A file capability.
pub enum File {}

/// Bytes inside the domain extent, represented as an offset rather than a
/// pointer on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Buffer(Region);

impl Buffer {
    /// Converts a live domain slice into the offset/length pair the ABI uses.
    pub fn from_slice(base: usize, bytes: &[u8]) -> Option<Self> {
        let address = bytes.as_ptr() as usize;
        let offset = address.checked_sub(base)?;
        Some(Self(Region::new(offset.try_into().ok()?, bytes.len().try_into().ok()?)))
    }

    /// Converts a writable domain slice into the offset/length pair the ABI uses.
    pub fn from_mut_slice(base: usize, bytes: &mut [u8]) -> Option<Self> {
        Self::from_slice(base, bytes)
    }
}

/// A protocol failure detected on the trusted user side.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    WrongCompletion,
}

/// The typed end of a domain ABI ring.
pub struct Client<'ring, const N: usize> {
    ring: Domain<'ring, N>,
    next: u64,
}

impl<'ring, const N: usize> Client<'ring, N> {
    /// Borrows the domain end of a channel mapped by the kernel.
    pub fn new(channel: &'ring Channel<N>) -> Self {
        Self { ring: channel.domain(), next: 1 }
    }

    /// Borrows a channel address supplied in the domain bootstrap registers.
    ///
    /// # Safety
    ///
    /// `channel` must point to an initialized, correctly aligned `Channel<N>`
    /// which stays mapped and shared with the kernel for `'ring`.
    pub unsafe fn from_raw(channel: *const ()) -> Self {
        // SAFETY: the caller supplies the validity, alignment, and lifetime.
        Self::new(unsafe { &*channel.cast::<Channel<N>>() })
    }

    pub fn timer(&mut self, ticks: u64) -> Request<'_, 'ring, N> {
        self.request(Op::Timer { ticks })
    }

    pub fn write(&mut self, output: Handle<Output>, bytes: Buffer) -> Request<'_, 'ring, N> {
        self.request(Op::Write { cap: output.raw, offset: 0, buf: bytes.0 })
    }

    pub fn close<T>(&mut self, handle: Handle<T>) -> Request<'_, 'ring, N> {
        self.request(Op::Close { cap: handle.raw })
    }

    pub fn open(&mut self, directory: Handle<Directory>, name: Buffer) -> Request<'_, 'ring, N> {
        self.request(Op::Open { dir: directory.raw, name: name.0 })
    }

    pub fn read(
        &mut self,
        file: Handle<File>,
        offset: u64,
        buffer: Buffer,
    ) -> Request<'_, 'ring, N> {
        self.request(Op::Read { cap: file.raw, offset, buf: buffer.0 })
    }

    fn request(&mut self, op: Op) -> Request<'_, 'ring, N> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        Request { client: self, id, call: Some(Call::new(id, op)) }
    }
}

/// One submitted request, completed by polling the paired completion ring.
pub struct Request<'client, 'ring, const N: usize> {
    client: &'client mut Client<'ring, N>,
    id: u64,
    call: Option<Call>,
}

impl<const N: usize> Future for Request<'_, '_, N> {
    type Output = Result<i64, Error>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(call) = self.call.take() {
            self.client.ring.submit(call);
        }
        match self.client.ring.reply() {
            Some(reply) if reply.id() == self.id => Poll::Ready(Ok(reply.result())),
            Some(_) => Poll::Ready(Err(Error::WrongCompletion)),
            None => {
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

/// Polls a domain future, crossing to the kernel whenever the ring needs work.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => boundary(KICK, 0),
        }
    }
}

/// Returns control to the kernel and never re-enters this image.
pub fn exit(status: i64) -> ! {
    boundary(EXIT, status as u64);
    loop {
        core::hint::spin_loop();
    }
}

fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);
    const fn raw() -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn clone(_: *const ()) -> RawWaker {
        raw()
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    // SAFETY: every vtable operation ignores the null data pointer, and clone
    // recreates the same inert waker.
    unsafe { Waker::from_raw(raw()) }
}

#[cfg(target_arch = "x86_64")]
fn boundary(reason: u64, value: u64) {
    // SAFETY: vector 0x80 is the Molt domain gate; the kernel preserves the
    // interrupted user context before returning or terminating it.
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") reason => _,
            in("rdi") value,
            clobber_abi("C"),
        );
    }
}

#[cfg(target_arch = "riscv64")]
fn boundary(reason: u64, value: u64) {
    // SAFETY: `ecall` is the RISC-V user/supervisor boundary. The kernel saves
    // all user registers before interpreting `a7` and `a0`.
    unsafe {
        asm!(
            "ecall",
            in("a7") reason,
            in("a0") value,
            clobber_abi("C"),
        );
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
fn boundary(_reason: u64, _value: u64) {
    unreachable!("molt-program supports x86_64 and riscv64")
}

/// Allocation over the domain heap supplied at entry.
///
/// Deallocation is intentionally a no-op: the whole heap is reclaimed when
/// the domain exits. This keeps allocator metadata inside the domain extent
/// and makes restart equivalent to dropping the extent.
pub struct Heap {
    next: AtomicUsize,
    end: AtomicUsize,
}

impl Heap {
    pub const fn empty() -> Self {
        Self { next: AtomicUsize::new(0), end: AtomicUsize::new(0) }
    }

    /// Supplies the one heap range for this domain instance.
    ///
    /// # Safety
    ///
    /// `start..start + len` must be writable memory owned exclusively by this
    /// allocator until the domain is destroyed, and this may be called once.
    pub unsafe fn initialize(&self, start: usize, len: usize) {
        self.end.store(start.saturating_add(len), Ordering::Release);
        self.next.store(start, Ordering::Release);
    }
}

// SAFETY: allocation claims disjoint ranges through one compare-exchange
// cursor. The caller of `initialize` supplies exclusively owned writable RAM.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut current = self.next.load(Ordering::Acquire);
        loop {
            let Some(start) =
                current.checked_add(layout.align() - 1).map(|at| at & !(layout.align() - 1))
            else {
                return null_mut();
            };
            let Some(end) = start.checked_add(layout.size()) else {
                return null_mut();
            };
            if end > self.end.load(Ordering::Acquire) {
                return null_mut();
            }
            match self.next.compare_exchange_weak(current, end, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return start as *mut u8,
                Err(observed) => current = observed,
            }
        }
    }

    /// Nothing is returned, deliberately.
    ///
    /// A domain's heap dies with the domain, and a program that outlives its
    /// heap wants an allocator with a free list rather than one that pretends.
    /// Making this a bump allocator and saying so is the honest version of the
    /// tradeoff; a long-lived domain is the point at which it stops being one.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll};

    use molt_abi::{Channel, Next, Reply};

    use super::{Client, noop_waker};

    async fn program<const N: usize>(client: &mut Client<'_, N>) -> i64 {
        client.timer(7).await.unwrap()
    }

    #[test]
    fn program_submits_and_awaits_without_wire_types() {
        let channel = Channel::<4>::new();
        let (mut submissions, mut completions) = channel.kernel();
        let mut client = Client::new(&channel);
        let mut running = pin!(program(&mut client));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert_eq!(running.as_mut().poll(&mut context), Poll::Pending);
        let Next::Ready(call) = submissions.take().unwrap() else {
            panic!("typed client did not submit a call");
        };
        completions.publish(Reply::new(call.id(), 19)).unwrap();
        assert_eq!(running.as_mut().poll(&mut context), Poll::Ready(19));
    }
}
