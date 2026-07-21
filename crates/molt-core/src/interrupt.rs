//! Bounded, lock-free interrupt arrivals a task can await.
//!
//! A completion carries a value produced once; an interrupt carries no value
//! and may arrive many times, including before anybody waits for it. So this is
//! not [`crate::completion`] with a different name: each slot is a monotonic
//! arrival counter plus an [`AtomicWaker`], and waiting means "observe the
//! counter move past the value it had when I armed the device".
//!
//! That shape is what makes the edge unloseable. The driver takes the snapshot
//! *before* it tells the device to interrupt, so a vector that fires between
//! arming and the first poll has already advanced the counter and the future
//! completes on its first look. A slab whose slot only held "pending / ready"
//! would have to be re-armed between arrivals, and the interrupt that landed in
//! that window would be gone.
//!
//! [`signal`](InterruptSlab::signal) is the only entry point interrupt context
//! uses. It takes an index rather than a token, because a hardware vector is
//! all a trap handler has, and it is wait-free: one `fetch_add` and one wake,
//! no claim to spin on and nothing to allocate.

use core::borrow::Borrow;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::cache::{CacheLayout, Compact, Padded};
use crate::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use crate::waker::AtomicWaker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptError {
    /// Every vector in the slab is already reserved.
    Full,
    /// That vector is owned by somebody else, or the slab has no such number.
    Taken,
    /// The vector was released, so nothing will ever signal this token again.
    Released,
}

/// A reserved vector. The index is the identity hardware is programmed with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptToken {
    index: usize,
    generation: u64,
}

impl InterruptToken {
    /// Which slab slot this token owns; the value an MSI message carries.
    pub const fn index(self) -> usize {
        self.index
    }
}

/// The slot is free and no token names it.
const FREE: u8 = 0;
/// A reservation is mid-flight; the generation is not yet published.
const RESERVING: u8 = 1;
/// The slot is reserved and a device may be programmed to signal it.
const LIVE: u8 = 2;

struct Slot {
    state: AtomicU8,
    /// Bumped on every reservation so a token cannot outlive its vector.
    generation: AtomicU64,
    /// Monotonic arrival count. Only ever increases, so a waiter compares
    /// rather than clears and no arrival can be consumed twice.
    count: AtomicU64,
    waker: AtomicWaker,
}

impl Slot {
    #[cfg(not(loom))]
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(FREE),
            generation: AtomicU64::new(0),
            count: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }

    #[cfg(loom)]
    fn new() -> Self {
        Self {
            state: AtomicU8::new(FREE),
            generation: AtomicU64::new(0),
            count: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }
}

/// Fixed-capacity table of interrupt vectors, indexed by hardware vector.
pub struct InterruptSlab<const N: usize, L: CacheLayout = Compact> {
    slots: [L::Slot<Slot>; N],
}

#[cfg(not(loom))]
impl<const N: usize> InterruptSlab<N, Compact> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "an interrupt slab needs at least one vector") };
        Self { slots: [const { Slot::new() }; N] }
    }
}

#[cfg(loom)]
impl<const N: usize> InterruptSlab<N, Compact> {
    pub fn new() -> Self {
        assert!(N > 0, "an interrupt slab needs at least one vector");
        Self { slots: core::array::from_fn(|_| Slot::new()) }
    }
}

#[cfg(not(loom))]
impl<const N: usize> InterruptSlab<N, Padded> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "an interrupt slab needs at least one vector") };
        Self { slots: [const { crate::cache::CachePadded::new(Slot::new()) }; N] }
    }
}

#[cfg(loom)]
impl<const N: usize> InterruptSlab<N, Padded> {
    pub fn new() -> Self {
        assert!(N > 0, "an interrupt slab needs at least one vector");
        Self { slots: core::array::from_fn(|_| crate::cache::CachePadded::new(Slot::new())) }
    }
}

impl<const N: usize, L: CacheLayout> InterruptSlab<N, L> {
    /// Reserves the lowest free vector.
    pub fn reserve(&self) -> Result<InterruptToken, InterruptError> {
        (0..N).find_map(|index| self.claim(index).ok()).ok_or(InterruptError::Full)
    }

    /// Reserves the vector at `index`, or [`InterruptError::Taken`] where it is
    /// already owned.
    ///
    /// Which number a device interrupts on is not the slab's to decide: on
    /// x86_64 it is an entry in the descriptor table the platform handed out,
    /// and the trap handler has nothing but that number to report. So a driver
    /// that has been given a vector claims *that* slot, and [`reserve`] is the
    /// special case of not caring which.
    ///
    /// [`reserve`]: Self::reserve
    pub fn claim(&self, index: usize) -> Result<InterruptToken, InterruptError> {
        let slot = self.slots.get(index).ok_or(InterruptError::Taken)?;
        let slot = slot.borrow();
        if slot
            .state
            .compare_exchange(FREE, RESERVING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(InterruptError::Taken);
        }
        let generation = slot.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // Release publishes the generation before any signal can observe
        // `LIVE` and count into it.
        slot.state.store(LIVE, Ordering::Release);
        Ok(InterruptToken { index, generation })
    }

    /// Frees the vector and wakes anything still waiting on it.
    ///
    /// The generation bump is what makes a token stale: a device that keeps
    /// signalling a released vector moves a counter nobody is reading, and a
    /// future holding the old token reports [`InterruptError::Released`]
    /// instead of waiting forever for an owner that no longer exists.
    pub fn release(&self, token: InterruptToken) -> Result<(), InterruptError> {
        let Some(slot) = self.slots.get(token.index) else {
            return Err(InterruptError::Released);
        };
        let slot = slot.borrow();
        if slot.state.load(Ordering::Acquire) != LIVE
            || slot.generation.load(Ordering::Acquire) != token.generation
        {
            return Err(InterruptError::Released);
        }
        // Bumping the generation before dropping to `FREE` closes the window in
        // which a re-reservation could hand the same generation back out.
        slot.generation.fetch_add(1, Ordering::AcqRel);
        let waker = slot.waker.take();
        slot.state.store(FREE, Ordering::Release);
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Records one arrival on `index` and wakes its waiter. Interrupt-context
    /// entry point: wait-free, and it never blocks on a claim.
    ///
    /// Returns `false` if no vector is reserved at `index`, which is how a
    /// device left programmed with a stale vector is detected rather than
    /// silently tolerated.
    pub fn signal(&self, index: usize) -> bool {
        let Some(slot) = self.slots.get(index) else {
            return false;
        };
        let slot = slot.borrow();
        let live = slot.state.load(Ordering::Acquire) == LIVE;
        // Count even when the vector is not live. A release racing this signal
        // would otherwise decide whether the arrival is recorded, and a
        // spurious count on a free slot is harmless: a waiter always compares
        // against a snapshot it took after its own reservation.
        slot.count.fetch_add(1, Ordering::Release);
        slot.waker.wake();
        live
    }

    /// Arms a wait on the next arrival after this call.
    ///
    /// Call this *before* telling the device to interrupt: the snapshot taken
    /// here is what makes an interrupt that beats the first poll observable.
    pub fn watch(&self, token: InterruptToken) -> InterruptFuture<'_, N, L> {
        let since = self
            .slots
            .get(token.index)
            .map_or(0, |slot| slot.borrow().count.load(Ordering::Acquire));
        InterruptFuture { slab: self, token, since }
    }

    /// Total arrivals recorded on the token's vector.
    pub fn arrivals(&self, token: InterruptToken) -> Result<u64, InterruptError> {
        let slot = self.slots.get(token.index).ok_or(InterruptError::Released)?;
        let slot = slot.borrow();
        if slot.generation.load(Ordering::Acquire) != token.generation {
            return Err(InterruptError::Released);
        }
        Ok(slot.count.load(Ordering::Acquire))
    }

    fn poll(
        &self,
        token: InterruptToken,
        since: u64,
        cx: &mut Context<'_>,
    ) -> Poll<Result<u64, InterruptError>> {
        let Some(slot) = self.slots.get(token.index) else {
            return Poll::Ready(Err(InterruptError::Released));
        };
        let slot = slot.borrow();
        if let Some(arrived) = Self::arrived(slot, token, since) {
            return Poll::Ready(arrived);
        }
        // Register and re-check, so an arrival landing between the check above
        // and the registration is not missed.
        slot.waker.register(cx.waker());
        match Self::arrived(slot, token, since) {
            Some(arrived) => Poll::Ready(arrived),
            None => Poll::Pending,
        }
    }

    /// Whether the vector moved past `since`, or lost its owner while waiting.
    fn arrived(
        slot: &Slot,
        token: InterruptToken,
        since: u64,
    ) -> Option<Result<u64, InterruptError>> {
        let count = slot.count.load(Ordering::Acquire);
        if slot.generation.load(Ordering::Acquire) != token.generation {
            return Some(Err(InterruptError::Released));
        }
        (count != since).then(|| Ok(count.wrapping_sub(since)))
    }
}

impl<const N: usize> Default for InterruptSlab<N, Compact> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Default for InterruptSlab<N, Padded> {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolves with the number of arrivals since [`InterruptSlab::watch`].
pub struct InterruptFuture<'s, const N: usize, L: CacheLayout = Compact> {
    slab: &'s InterruptSlab<N, L>,
    token: InterruptToken,
    since: u64,
}

impl<const N: usize, L: CacheLayout> Future for InterruptFuture<'_, N, L> {
    type Output = Result<u64, InterruptError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.slab.poll(self.token, self.since, cx)
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use core::task::{Context, Poll, Waker};

    use loom::sync::Arc;
    use loom::thread;

    use super::InterruptSlab;
    use crate::waker::Flag;

    /// The property the whole module exists for: an interrupt that fires while
    /// a task is parking must not leave it parked.
    #[test]
    fn race_keeps_arrival() {
        loom::model(|| {
            let slab = Arc::new(InterruptSlab::<1>::new());
            let token = slab.reserve().expect("free vector");
            let future = slab.watch(token);
            let flag = Flag::new();
            let waker = Waker::from(flag.clone());

            let device = {
                let slab = slab.clone();
                thread::spawn(move || slab.signal(token.index()))
            };
            let polled = slab.poll(token, 0, &mut Context::from_waker(&waker));
            device.join().unwrap();
            drop(future);

            match polled {
                Poll::Ready(result) => assert_eq!(result, Ok(1)),
                Poll::Pending => assert!(flag.fired(), "parked without a wake"),
            }
        });
    }

    /// A release racing an interrupt must end the wait either way: with the
    /// arrival if it was counted in time, otherwise by reporting the vector
    /// gone. Staying pending would strand the task.
    #[test]
    fn race_ends_wait() {
        loom::model(|| {
            let slab = Arc::new(InterruptSlab::<1>::new());
            let token = slab.reserve().expect("free vector");
            let flag = Flag::new();
            let waker = Waker::from(flag.clone());

            let device = {
                let slab = slab.clone();
                thread::spawn(move || slab.signal(token.index()))
            };
            let released = slab.release(token);
            let polled = slab.poll(token, 0, &mut Context::from_waker(&waker));
            device.join().unwrap();

            if released.is_ok() {
                assert!(polled.is_ready() || flag.fired(), "released without a wake");
            } else {
                assert_eq!(polled, Poll::Ready(Ok(1)));
            }
        });
    }
}
