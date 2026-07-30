//! Deadlines in a hierarchical wheel: four levels of sixty-four slots.
//!
//! Arming is a shift and a push, and a tick looks at one slot instead of every
//! deadline. What a level cannot express it hands upward — a deadline more than
//! sixty-four ticks out waits in level one, which cascades into level zero when
//! the lower wheel wraps, so a timer moves a bounded number of times no matter
//! how far ahead it was set.
//!
//! Ticks are the machine's, and the wheel walks one slot per tick: the unit
//! should be the scheduling quantum, not a cycle counter. The clock is read
//! from the machine and never from the wheel, which only trails it — a
//! deadline taken off a wheel nobody has advanced yet is one already past.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::machine::Machine;

const BITS: usize = 6;
const SLOTS: usize = 1 << BITS;
const LEVELS: usize = 4;
const MASK: u64 = SLOTS as u64 - 1;

/// How far ahead a deadline is kept exactly. Anything beyond is clamped to it.
pub const HORIZON: u64 = 1 << (BITS * LEVELS);

struct Timer {
    deadline: u64,
    expired: Cell<bool>,
    waker: Cell<Option<Waker>>,
}

impl Timer {
    fn new(deadline: u64) -> Rc<Self> {
        Rc::new(Self { deadline, expired: Cell::new(false), waker: Cell::new(None) })
    }

    fn expire(&self) -> Option<Waker> {
        self.expired.set(true);
        self.waker.take()
    }
}

struct Wheel {
    now: u64,
    slots: [[Vec<Rc<Timer>>; SLOTS]; LEVELS],
}

impl Wheel {
    fn new() -> Self {
        Self { now: 0, slots: [const { [const { Vec::new() }; SLOTS] }; LEVELS] }
    }

    fn arm(&mut self, deadline: u64) -> Rc<Timer> {
        let deadline = deadline.min(self.now + HORIZON - 1);
        let timer = Timer::new(deadline);
        if deadline <= self.now {
            timer.expired.set(true);
        } else {
            self.place(timer.clone());
        }
        timer
    }

    /// Files a timer by how far off it is: the coarser the wheel, the further.
    fn place(&mut self, timer: Rc<Timer>) {
        let delta = timer.deadline.saturating_sub(self.now);
        let level =
            (0..LEVELS).find(|level| delta < 1 << (BITS * (level + 1))).unwrap_or(LEVELS - 1);
        let slot = ((timer.deadline >> (BITS * level)) & MASK) as usize;
        self.slots[level][slot].push(timer);
    }

    fn upto(&mut self, ticks: u64, expired: &mut Vec<Waker>) {
        if ticks.saturating_sub(self.now) >= HORIZON {
            self.flush(expired);
            self.now = ticks;
            return;
        }
        while self.now < ticks {
            self.step(expired);
        }
    }

    fn step(&mut self, expired: &mut Vec<Waker>) {
        self.now += 1;
        // A wrapped level owes the one below it whatever it was holding, and a
        // level only wraps once the one under it did.
        for level in 1..LEVELS {
            if (self.now >> (BITS * (level - 1))) & MASK != 0 {
                break;
            }
            let slot = ((self.now >> (BITS * level)) & MASK) as usize;
            let mut due = mem::take(&mut self.slots[level][slot]);
            for timer in due.drain(..) {
                self.place(timer);
            }
            self.slots[level][slot] = due;
        }

        let slot = (self.now & MASK) as usize;
        let mut due = mem::take(&mut self.slots[0][slot]);
        expired.extend(due.drain(..).filter_map(|timer| timer.expire()));
        self.slots[0][slot] = due;
    }

    /// Past the horizon nothing is still pending, so the wheel empties whole.
    fn flush(&mut self, expired: &mut Vec<Waker>) {
        for level in &mut self.slots {
            for slot in level {
                expired.extend(slot.drain(..).filter_map(|timer| timer.expire()));
            }
        }
    }
}

/// One core's deadlines. Not shared, not sent — the wheel belongs to its core.
#[derive(Clone)]
pub struct Timers(Rc<Inner>);

struct Inner {
    machine: &'static dyn Machine,
    wheel: RefCell<Wheel>,
    /// Kept between turns so waking a hundred timers costs no allocation.
    expired: RefCell<Vec<Waker>>,
}

impl Timers {
    pub fn new(machine: &'static dyn Machine) -> Self {
        Self(Rc::new(Inner {
            machine,
            wheel: RefCell::new(Wheel::new()),
            expired: RefCell::new(Vec::new()),
        }))
    }

    /// The machine's clock, which the wheel trails until the next [`advance`].
    ///
    /// [`advance`]: Timers::advance
    pub fn now(&self) -> u64 {
        self.0.machine.ticks()
    }

    /// Sleeps `delay` ticks from now.
    pub fn after(&self, delay: u64) -> Sleep {
        let deadline = self.now().saturating_add(delay);
        self.until(deadline)
    }

    /// Sleeps until the tick `deadline`, or returns ready if it has passed.
    pub fn until(&self, deadline: u64) -> Sleep {
        Sleep { timer: self.0.wheel.borrow_mut().arm(deadline) }
    }

    /// Runs `future` for at most `delay` ticks.
    pub fn timeout<F: Future>(&self, delay: u64, future: F) -> Timeout<F> {
        Timeout { future, sleep: self.after(delay) }
    }

    /// Walks the wheel to the clock and wakes what expired, reporting how many.
    pub fn advance(&self) -> usize {
        // The scratch leaves the cell for the walk, so a waker that comes back
        // here finds nothing borrowed.
        let mut expired = mem::take(&mut *self.0.expired.borrow_mut());
        self.0.wheel.borrow_mut().upto(self.now(), &mut expired);
        let woken = expired.len();
        for waker in expired.drain(..) {
            waker.wake();
        }
        *self.0.expired.borrow_mut() = expired;
        woken
    }
}

/// A deadline, waiting.
///
/// Dropping one stops the wake but leaves the slot: the wheel forgets it when
/// the tick it was armed for comes round, and not before.
pub struct Sleep {
    timer: Rc<Timer>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.timer.expired.get() {
            return Poll::Ready(());
        }
        self.timer.waker.set(Some(context.waker().clone()));
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.timer.waker.take();
    }
}

/// A future and a deadline: `None` is the deadline arriving first.
///
/// What a driver awaits is a device, and a device can be wedged. This is what
/// gives up on one without a spin count — the wheel says when, and the core
/// sleeps meanwhile.
pub struct Timeout<F> {
    future: F,
    sleep: Sleep,
}

impl<F: Future> Future for Timeout<F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: pinning reaches the future through this, and nothing here
        // moves it. `Sleep` needs no such care, being `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: as above.
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        if let Poll::Ready(value) = future.poll(context) {
            return Poll::Ready(Some(value));
        }
        Pin::new(&mut this.sleep).poll(context).map(|()| None)
    }
}
