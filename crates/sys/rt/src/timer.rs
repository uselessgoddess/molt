//! Deadlines in a hierarchical wheel: four levels of sixty-four slots.
//!
//! Arming is a shift and a link, and a tick looks at one slot instead of every
//! deadline. What a level cannot express it hands upward — a deadline more than
//! sixty-four ticks out waits in level one, which cascades into level zero when
//! the lower wheel wraps, so a timer moves a bounded number of times no matter
//! how far ahead it was set.
//!
//! The timers themselves live in a slab and are linked through it by index, so
//! arming takes a slot off a free chain and cancelling is a handful of writes:
//! no allocation, no scan, no reference count. A slot belongs to the one
//! [`Sleep`] that armed it until that drops, which is also what frees it, so an
//! index cannot go stale while anything can still use it — there is nothing
//! here for a generation counter to catch.
//!
//! Ticks are the machine's, and the wheel walks one slot per tick: the unit
//! should be the scheduling quantum, not a cycle counter. The clock is read
//! from the machine and never from the wheel, which only trails it — a
//! deadline taken off a wheel nobody has advanced yet is one already past.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::machine::Machine;

const BITS: usize = 6;
const SLOTS: usize = 1 << BITS;
const LEVELS: usize = 4;
const MASK: u64 = SLOTS as u64 - 1;
const BUCKETS: usize = SLOTS * LEVELS;

/// No slot: an empty bucket, the end of a chain, a timer in neither.
const NONE: u32 = u32::MAX;

/// How far ahead a deadline is kept exactly. Anything beyond is clamped to it.
pub const HORIZON: u64 = 1 << (BITS * LEVELS);

/// A deadline, linked into a bucket or into the free chain.
struct Node {
    deadline: u64,
    waker: Option<Waker>,
    prev: u32,
    next: u32,
    /// Where it is filed, or `NONE` once it has fired.
    bucket: u32,
}

impl Node {
    fn fired(&self) -> bool {
        self.bucket == NONE
    }
}

struct Wheel {
    now: u64,
    nodes: Vec<Node>,
    /// Head of the chain of slots to reuse, through `next`.
    free: u32,
    heads: [u32; BUCKETS],
}

impl Wheel {
    fn new() -> Self {
        Self { now: 0, nodes: Vec::new(), free: NONE, heads: [NONE; BUCKETS] }
    }

    fn arm(&mut self, deadline: u64) -> u32 {
        let deadline = deadline.min(self.now + HORIZON - 1);
        let slot = self.take(deadline);
        if deadline > self.now {
            self.place(slot);
        }
        slot
    }

    /// A slot off the free chain, or a fresh one when it is empty.
    fn take(&mut self, deadline: u64) -> u32 {
        let node = Node { deadline, waker: None, prev: NONE, next: NONE, bucket: NONE };
        if self.free == NONE {
            self.nodes.push(node);
            return (self.nodes.len() - 1) as u32;
        }
        let slot = self.free;
        self.free = self.nodes[slot as usize].next;
        self.nodes[slot as usize] = node;
        slot
    }

    /// Gives a slot back, whether it fired or was cancelled.
    fn release(&mut self, slot: u32) {
        if !self.nodes[slot as usize].fired() {
            self.unlink(slot);
        }
        let node = &mut self.nodes[slot as usize];
        node.waker = None;
        node.next = self.free;
        self.free = slot;
    }

    /// Files a timer by how far off it is: the coarser the wheel, the further.
    fn place(&mut self, slot: u32) {
        let deadline = self.nodes[slot as usize].deadline;
        let delta = deadline.saturating_sub(self.now);
        let level =
            (0..LEVELS).find(|level| delta < 1 << (BITS * (level + 1))).unwrap_or(LEVELS - 1);
        let bucket = level * SLOTS + ((deadline >> (BITS * level)) & MASK) as usize;
        self.link(slot, bucket as u32);
    }

    fn link(&mut self, slot: u32, bucket: u32) {
        let head = self.heads[bucket as usize];
        let node = &mut self.nodes[slot as usize];
        node.bucket = bucket;
        node.prev = NONE;
        node.next = head;
        if head != NONE {
            self.nodes[head as usize].prev = slot;
        }
        self.heads[bucket as usize] = slot;
    }

    fn unlink(&mut self, slot: u32) {
        let node = &self.nodes[slot as usize];
        let (bucket, prev, next) = (node.bucket, node.prev, node.next);
        if prev == NONE {
            self.heads[bucket as usize] = next;
        } else {
            self.nodes[prev as usize].next = next;
        }
        if next != NONE {
            self.nodes[next as usize].prev = prev;
        }
        self.cut(slot);
    }

    /// Clears one node's links, reporting what followed it. The bucket is left
    /// to the caller, which is why this is only for a chain taken whole.
    fn cut(&mut self, slot: u32) -> u32 {
        let node = &mut self.nodes[slot as usize];
        let next = node.next;
        node.bucket = NONE;
        node.prev = NONE;
        node.next = NONE;
        next
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
            let bucket = level * SLOTS + ((self.now >> (BITS * level)) & MASK) as usize;
            // The whole chain moves, so the bucket empties in one write and
            // each node pays for its links and nothing else.
            let mut slot = mem::replace(&mut self.heads[bucket], NONE);
            while slot != NONE {
                let next = self.cut(slot);
                self.place(slot);
                slot = next;
            }
        }

        self.expire((self.now & MASK) as usize, expired);
    }

    /// Empties a bucket, taking the wakers of everything that was in it. The
    /// slots stay, because their sleepers still hold them.
    fn expire(&mut self, bucket: usize, expired: &mut Vec<Waker>) {
        let mut slot = mem::replace(&mut self.heads[bucket], NONE);
        while slot != NONE {
            let next = self.cut(slot);
            expired.extend(self.nodes[slot as usize].waker.take());
            slot = next;
        }
    }

    /// Past the horizon nothing is still pending, so the wheel empties whole.
    fn flush(&mut self, expired: &mut Vec<Waker>) {
        for bucket in 0..BUCKETS {
            self.expire(bucket, expired);
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
        let slot = self.0.wheel.borrow_mut().arm(deadline);
        Sleep { timers: self.clone(), slot }
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
/// Dropping one stops the wake and hands the slot back, so a timeout that was
/// not needed costs the wheel nothing until the deadline it was armed for.
pub struct Sleep {
    timers: Timers,
    slot: u32,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut wheel = self.timers.0.wheel.borrow_mut();
        let node = &mut wheel.nodes[self.slot as usize];
        if node.fired() {
            return Poll::Ready(());
        }
        node.waker = Some(context.waker().clone());
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.timers.0.wheel.borrow_mut().release(self.slot);
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
