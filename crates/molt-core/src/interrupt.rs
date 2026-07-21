//! Interrupt arrivals, delivered to a waiting task without a lock.
//!
//! [`crate::completion`] answers "this one request finished". An interrupt is
//! the other shape: the same line fires again and again, the handler runs in a
//! context that may not block, and an arrival that lands before anyone is
//! waiting must not be thrown away. So a line here is a *counter*, not a slot.
//! [`InterruptSlab::raise`] increments it and fires the parked waker;
//! [`InterruptSlab::wait`] reports how many arrivals happened since the owner
//! last looked.
//!
//! Counting rather than flagging is what makes coalescing honest. Two
//! interrupts that arrive between two polls are reported as two arrivals, and a
//! driver that only needs "something happened" can ignore the number. Nothing
//! silently disappears.
//!
//! A line is claimed with [`bind`](InterruptSlab::bind) and released with
//! [`release`](InterruptSlab::release), which bumps a generation. That is what
//! makes a stale interrupt harmless: a device left programmed with a released
//! vector keeps writing it, and the counter keeps moving, but the token that
//! could read it no longer matches. Reusing the line hands the new owner a
//! fresh baseline rather than the previous owner's backlog.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::sync::atomic::{AtomicU64, Ordering};
use crate::waker::AtomicWaker;

/// Why an interrupt line refused an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptError {
    /// The line is outside the slab.
    Range,
    /// The line already has an owner.
    Bound,
    /// The line was released, or rebound, since this token was issued.
    Stale,
}

/// Proof that its holder owns one interrupt line.
///
/// Carries the generation the line had when it was bound, so a token that
/// outlived its binding is rejected instead of reading someone else's device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptToken {
    line: u16,
    generation: u64,
}

impl InterruptToken {
    /// The line this token owns, as the platform's interrupt path names it.
    pub const fn line(self) -> u16 {
        self.line
    }
}

/// One interrupt line: an arrival counter, its owner's baseline, and a waker.
struct Line {
    /// Arrivals since boot. Only ever incremented, and only by `raise`.
    arrivals: AtomicU64,
    /// The `arrivals` value the owner has already been told about.
    acknowledged: AtomicU64,
    /// Even when free, odd when bound; bumped by every bind and release.
    generation: AtomicU64,
    waker: AtomicWaker,
}

impl Line {
    #[cfg(not(loom))]
    const fn new() -> Self {
        Self {
            arrivals: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }

    #[cfg(loom)]
    fn new() -> Self {
        Self {
            arrivals: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }

    /// Reports the arrivals `generation`'s owner has not yet seen.
    fn take(&self, generation: u64) -> Poll<Result<u64, InterruptError>> {
        if self.generation.load(Ordering::Acquire) != generation {
            return Poll::Ready(Err(InterruptError::Stale));
        }
        let arrivals = self.arrivals.load(Ordering::Acquire);
        let acknowledged = self.acknowledged.load(Ordering::Acquire);
        if arrivals == acknowledged {
            return Poll::Pending;
        }
        self.acknowledged.store(arrivals, Ordering::Release);
        Poll::Ready(Ok(arrivals - acknowledged))
    }
}

/// A fixed bank of interrupt lines shared with interrupt context.
///
/// `N` is the number of lines the platform reserved for devices. The slab lives
/// in a `static`: the interrupt entry path has no other way to reach it.
pub struct InterruptSlab<const N: usize> {
    lines: [Line; N],
}

#[cfg(not(loom))]
impl<const N: usize> InterruptSlab<N> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "an interrupt slab needs at least one line") };
        const { assert!(N <= u16::MAX as usize, "interrupt lines are addressed by a u16") };
        Self { lines: [const { Line::new() }; N] }
    }
}

#[cfg(loom)]
impl<const N: usize> InterruptSlab<N> {
    pub fn new() -> Self {
        assert!(N > 0, "an interrupt slab needs at least one line");
        Self { lines: core::array::from_fn(|_| Line::new()) }
    }
}

impl<const N: usize> InterruptSlab<N> {
    /// Records that `line` fired. Safe to call from interrupt context.
    ///
    /// Wait-free, allocation-free, and deliberately tolerant: a line nobody
    /// owns still counts, because refusing here would mean deciding in an
    /// interrupt handler what to do about a device that should not have fired.
    pub fn raise(&self, line: u16) {
        let Some(line) = self.lines.get(line as usize) else {
            return;
        };
        line.arrivals.fetch_add(1, Ordering::Release);
        line.waker.wake();
    }

    /// Claims `line` for one owner.
    ///
    /// The new owner starts level with the counter, so arrivals that belong to
    /// whatever held the line before are not delivered to it.
    pub fn bind(&self, line: u16) -> Result<InterruptToken, InterruptError> {
        let entry = self.lines.get(line as usize).ok_or(InterruptError::Range)?;
        let free = entry.generation.load(Ordering::Acquire);
        if free % 2 == 1 {
            return Err(InterruptError::Bound);
        }
        entry
            .generation
            .compare_exchange(free, free + 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| InterruptError::Bound)?;
        entry.acknowledged.store(entry.arrivals.load(Ordering::Acquire), Ordering::Release);
        Ok(InterruptToken { line, generation: free + 1 })
    }

    /// Gives up a line. The caller must have stopped the device first.
    pub fn release(&self, token: InterruptToken) -> Result<(), InterruptError> {
        let entry = self.line(token)?;
        entry
            .generation
            .compare_exchange(
                token.generation,
                token.generation + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| InterruptError::Stale)?;
        // Drops any parked waker: the future that registered it will observe
        // the new generation and resolve `Stale` on its next poll.
        let _ = entry.waker.take();
        Ok(())
    }

    /// Reports arrivals since the last successful read, without waiting.
    pub fn arrivals(&self, token: InterruptToken) -> Result<u64, InterruptError> {
        match self.line(token)?.take(token.generation) {
            Poll::Ready(result) => result,
            Poll::Pending => Ok(0),
        }
    }

    /// Resolves when the line has fired at least once since it was last read.
    pub fn wait(&self, token: InterruptToken) -> InterruptFuture<'_, N> {
        InterruptFuture { slab: self, token }
    }

    fn line(&self, token: InterruptToken) -> Result<&Line, InterruptError> {
        self.lines.get(token.line as usize).ok_or(InterruptError::Range)
    }
}

#[cfg(not(loom))]
impl<const N: usize> Default for InterruptSlab<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Waits for the next arrival on a bound line.
///
/// Resolves to the number of arrivals coalesced into this wakeup, or
/// [`InterruptError::Stale`] if the line was released while waiting.
pub struct InterruptFuture<'slab, const N: usize> {
    slab: &'slab InterruptSlab<N>,
    token: InterruptToken,
}

impl<const N: usize> Future for InterruptFuture<'_, N> {
    type Output = Result<u64, InterruptError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let Ok(line) = self.slab.line(self.token) else {
            return Poll::Ready(Err(InterruptError::Range));
        };
        if let Poll::Ready(result) = line.take(self.token.generation) {
            return Poll::Ready(result);
        }
        line.waker.register(context.waker());
        // An interrupt between the check above and the registration would have
        // found no waker to fire, so the count is read again afterwards.
        line.take(self.token.generation)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use super::{InterruptError, InterruptSlab};

    fn context() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    #[test]
    fn arrival_before_the_wait_is_not_lost() {
        let slab = InterruptSlab::<2>::new();
        let token = slab.bind(0).expect("a free line");

        slab.raise(0);

        assert_eq!(pin!(slab.wait(token)).poll(&mut context()), Poll::Ready(Ok(1)));
    }

    #[test]
    fn arrivals_coalesce_into_one_wakeup() {
        let slab = InterruptSlab::<2>::new();
        let token = slab.bind(1).expect("a free line");

        slab.raise(1);
        slab.raise(1);
        slab.raise(1);

        assert_eq!(slab.arrivals(token), Ok(3));
        assert_eq!(slab.arrivals(token), Ok(0), "arrivals were reported twice");
    }

    #[test]
    fn a_line_has_one_owner() {
        let slab = InterruptSlab::<1>::new();
        let token = slab.bind(0).expect("a free line");

        assert_eq!(slab.bind(0), Err(InterruptError::Bound));

        slab.release(token).expect("the line this token bound");
        assert!(slab.bind(0).is_ok(), "a released line stayed bound");
    }

    #[test]
    fn a_released_token_reads_nothing() {
        let slab = InterruptSlab::<1>::new();
        let token = slab.bind(0).expect("a free line");
        slab.release(token).expect("the line this token bound");

        slab.raise(0);

        assert_eq!(slab.arrivals(token), Err(InterruptError::Stale));
        assert_eq!(slab.release(token), Err(InterruptError::Stale));
    }

    #[test]
    fn a_rebound_line_starts_level() {
        let slab = InterruptSlab::<1>::new();
        let stale = slab.bind(0).expect("a free line");
        slab.raise(0);
        slab.release(stale).expect("the line this token bound");

        let token = slab.bind(0).expect("the released line");

        assert_eq!(slab.arrivals(token), Ok(0), "the new owner inherited a backlog");
        assert_eq!(pin!(slab.wait(token)).poll(&mut context()), Poll::Pending);
    }

    #[test]
    fn a_line_outside_the_slab_is_refused() {
        let slab = InterruptSlab::<1>::new();

        assert_eq!(slab.bind(1), Err(InterruptError::Range));
        slab.raise(1);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use loom::sync::Arc;
    use loom::thread;

    use super::InterruptSlab;
    use crate::waker::Flag;

    /// An interrupt racing a poll must never leave the task parked: either the
    /// poll sees the arrival, or the waker it registered is fired.
    #[test]
    fn race_delivers_arrival() {
        loom::model(|| {
            let slab = Arc::new(InterruptSlab::<1>::new());
            let token = slab.bind(0).expect("a free line");
            let flag = Flag::new();
            let waker = Waker::from(flag.clone());

            let device = {
                let slab = slab.clone();
                thread::spawn(move || slab.raise(0))
            };
            let polled = pin!(slab.wait(token)).poll(&mut Context::from_waker(&waker));
            device.join().unwrap();

            assert!(
                matches!(polled, Poll::Ready(Ok(_))) || flag.fired(),
                "the arrival neither resolved the future nor woke it"
            );
        });
    }
}
