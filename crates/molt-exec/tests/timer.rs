use std::future::{Future, pending, ready};
use std::ops::Deref;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

use molt_exec::{HORIZON, Machine, Sleep, Timers};

#[test]
fn fires_at_deadline() {
    let timers = wound(0);
    let mut sleep = pin!(timers.after(3));

    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timers.walk(2), 0);
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.walk(3), 1);
    assert!(poll(sleep.as_mut()).is_ready());
    assert_eq!(timers.now(), 3);
}

#[test]
fn cascades() {
    let timers = wound(0);
    let mut sleep = pin!(timers.after(100));
    assert!(poll(sleep.as_mut()).is_pending());

    // Armed at level one, walked down when the lower wheel wrapped.
    assert_eq!(timers.walk(64), 0);
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.walk(100), 1);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn keeps_order() {
    let timers = wound(0);
    let mut early = pin!(timers.after(1));
    let mut late = pin!(timers.after(4_100));
    assert!(poll(early.as_mut()).is_pending());
    assert!(poll(late.as_mut()).is_pending());

    assert_eq!(timers.walk(1), 1);
    assert!(poll(early.as_mut()).is_ready());
    assert!(poll(late.as_mut()).is_pending());

    assert_eq!(timers.walk(4_100), 1);
    assert!(poll(late.as_mut()).is_ready());
}

#[test]
fn past_horizon_expires() {
    let timers = wound(0);
    let mut sleep = pin!(timers.after(HORIZON * 4));
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.walk(HORIZON), 1);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn elapsed_is_ready() {
    let timers = wound(10);

    assert!(poll(pin!(timers.until(4)).as_mut()).is_ready());
    assert!(poll(pin!(timers.after(0)).as_mut()).is_ready());
}

/// A deadline is the clock's, not the wheel's: armed while the wheel still
/// trails, it waits the whole delay out rather than expiring on the catch-up.
#[test]
fn arms_off_clock() {
    let timers = wound(0);
    timers.clock.0.store(200, Ordering::Release);
    let mut sleep = pin!(timers.after(3));

    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timers.walk(202), 0);
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.walk(203), 1);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn drop_stops_wake() {
    let timers = wound(0);
    {
        let mut sleep = pin!(timers.after(2));
        assert!(poll(sleep.as_mut()).is_pending());
    }
    assert_eq!(timers.walk(2), 0);
}

#[test]
fn timeout_gives_up() {
    let timers = wound(0);
    let mut wedged = pin!(timers.timeout(2, pending::<u32>()));
    assert!(step(wedged.as_mut()).is_pending());

    assert_eq!(timers.walk(2), 1);

    assert_eq!(step(wedged.as_mut()), Poll::Ready(None));
}

#[test]
fn timeout_keeps_value() {
    let timers = wound(0);
    let mut answered = pin!(timers.timeout(2, ready(7)));

    assert_eq!(step(answered.as_mut()), Poll::Ready(Some(7)));
}

/// A machine whose only trait is a clock the test winds.
struct Clock(AtomicU64);

impl Machine for Clock {
    fn ticks(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// A wheel and the clock under it, one pair per test.
struct Wound {
    clock: &'static Clock,
    timers: Timers,
}

impl Wound {
    /// Winds the clock to `ticks` and walks the wheel there, reporting what
    /// expired on the way.
    fn walk(&self, ticks: u64) -> usize {
        self.clock.0.store(ticks, Ordering::Release);
        self.timers.advance()
    }
}

impl Deref for Wound {
    type Target = Timers;

    fn deref(&self) -> &Timers {
        &self.timers
    }
}

fn wound(ticks: u64) -> Wound {
    let clock: &'static Clock = Box::leak(Box::new(Clock(AtomicU64::new(ticks))));
    let timers = Timers::new(clock);
    timers.advance();
    Wound { clock, timers }
}

fn poll(sleep: Pin<&mut Sleep>) -> Poll<()> {
    sleep.poll(&mut Context::from_waker(Waker::noop()))
}

fn step<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
