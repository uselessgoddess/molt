//! What the hot paths cost, counted instead of timed.
//!
//! A shared runner has opinions about nanoseconds; it has none about how many
//! allocations a wake takes, or how many doorbells a burst rings. These are the
//! numbers a change may not quietly spend, so a regression fails here rather
//! than showing up as a slower graph nobody trusts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::pending;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use molt_core::cpu::CpuId;
use molt_exec::{Executor, Machine, Sleep, Solo, SpawnError, Timers};

#[global_allocator]
static ALLOC: Counter = Counter;

static SOLO: Solo = Solo;
static CLOCK: Clock = Clock(AtomicU64::new(0));
static BELL: Bell = Bell(AtomicUsize::new(0));

/// Steady state is what is measured, so the queues get their growth first.
const WARM: usize = 64;

#[test]
fn wake_is_free() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 4);
    let pulse = Pulse::default();
    executor.spawn(pulse.clone())?;
    pulse.arm(WARM);
    executor.run_until_idle();

    pulse.arm(WARM);
    // One poll per wake, and one more that finds no budget left.
    assert_eq!(cost(|| assert_eq!(executor.run_until_idle(), WARM + 1)), 0);
    Ok(())
}

/// A cancelled deadline gives its slot back, so the next arm costs nothing.
#[test]
fn arm_reuses_slot() {
    let timers = Timers::new(&SOLO);
    let arm = || {
        for _ in 0..WARM {
            drop(timers.after(1_000));
        }
    };
    arm();

    assert_eq!(cost(arm), 0);
}

#[test]
fn expiry_is_free() {
    let timers = Timers::new(&CLOCK);
    let mut sleeps = Vec::with_capacity(WARM);
    for tick in 1..=WARM as u64 {
        round(&timers, &mut sleeps, tick * 8);
    }

    assert_eq!(cost(|| round(&timers, &mut sleeps, (WARM as u64 + 1) * 8)), 0);
}

/// Room for `capacity` tasks is taken once, so no spawn pays for the next.
#[test]
fn spawn_is_flat() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, WARM);
    let mut costs = Vec::with_capacity(WARM);

    for _ in 0..WARM {
        costs.push(cost(|| {
            executor.spawn(pending()).unwrap();
            executor.run_until_idle();
        }));
    }

    assert!(costs.windows(2).all(|pair| pair[0] == pair[1]), "{costs:?}");
    Ok(())
}

/// The owner drains the inbox whole, so a burst filling it rings once.
#[test]
fn burst_rings_once() -> Result<(), SpawnError> {
    let executor = Executor::new(&BELL, WARM + 1);
    for _ in 0..WARM {
        executor.spawn(pending())?;
    }
    assert_eq!(BELL.count(), 1);

    // Emptied, so the next one owes a ring again.
    executor.run_until_idle();
    executor.spawn(pending())?;

    assert_eq!(BELL.count(), 2);
    Ok(())
}

/// Arms a batch, walks the clock past it, and drops what expired.
fn round(timers: &Timers, sleeps: &mut Vec<Sleep>, tick: u64) {
    sleeps.extend((0..WARM).map(|_| timers.after(4)));
    CLOCK.0.store(tick, Ordering::Release);
    timers.advance();
    sleeps.clear();
}

/// Allocations `body` makes, on this thread and while it runs.
fn cost(body: impl FnOnce()) -> usize {
    let before = COUNT.with(Cell::get);
    body();
    COUNT.with(Cell::get) - before
}

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, keeping a tally per thread.
struct Counter;

// SAFETY: every request is handed to the system allocator unchanged; the tally
// is a thread's own and allocates nothing itself.
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.try_with(|count| count.set(count.get() + 1)).ok();
        // SAFETY: the caller's layout, passed straight on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as above, and the pointer is one this allocator returned.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        COUNT.try_with(|count| count.set(count.get() + 1)).ok();
        // SAFETY: as above.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

/// A machine whose only trait is a clock the test winds.
struct Clock(AtomicU64);

impl Machine for Clock {
    fn ticks(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// A machine that counts its doorbell instead of ringing one.
struct Bell(AtomicUsize);

impl Bell {
    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

impl Machine for Bell {
    fn wake(&self, _: CpuId) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// A task that wakes itself as many times as it is given, and never finishes.
#[derive(Clone, Default)]
struct Pulse(Rc<Beat>);

#[derive(Default)]
struct Beat {
    budget: Cell<usize>,
    waker: Cell<Option<Waker>>,
}

impl Pulse {
    /// Grants `wakes` more turns and starts them off.
    fn arm(&self, wakes: usize) {
        self.0.budget.set(wakes);
        if let Some(waker) = self.0.waker.take() {
            waker.wake();
        }
    }
}

impl Future for Pulse {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        self.0.waker.set(Some(context.waker().clone()));
        if let Some(left) = self.0.budget.get().checked_sub(1) {
            self.0.budget.set(left);
            context.waker().wake_by_ref();
        }
        Poll::Pending
    }
}
