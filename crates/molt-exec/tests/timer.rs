use std::future::{Future, pending, ready};
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

use molt_exec::{HORIZON, Sleep, Timers};

#[test]
fn fires_at_deadline() {
    let timers = Timers::new();
    let mut sleep = pin!(timers.after(3));

    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timers.advance(2), 0);
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.advance(3), 1);
    assert!(poll(sleep.as_mut()).is_ready());
    assert_eq!(timers.now(), 3);
}

#[test]
fn cascades() {
    let timers = Timers::new();
    let mut sleep = pin!(timers.after(100));
    assert!(poll(sleep.as_mut()).is_pending());

    // Armed at level one, walked down when the lower wheel wrapped.
    assert_eq!(timers.advance(64), 0);
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.advance(100), 1);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn keeps_order() {
    let timers = Timers::new();
    let mut early = pin!(timers.after(1));
    let mut late = pin!(timers.after(4_100));
    assert!(poll(early.as_mut()).is_pending());
    assert!(poll(late.as_mut()).is_pending());

    assert_eq!(timers.advance(1), 1);
    assert!(poll(early.as_mut()).is_ready());
    assert!(poll(late.as_mut()).is_pending());

    assert_eq!(timers.advance(4_100), 1);
    assert!(poll(late.as_mut()).is_ready());
}

#[test]
fn past_horizon_expires() {
    let timers = Timers::new();
    let mut sleep = pin!(timers.after(HORIZON * 4));
    assert!(poll(sleep.as_mut()).is_pending());

    assert_eq!(timers.advance(HORIZON), 1);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn elapsed_is_ready() {
    let timers = Timers::new();
    timers.advance(10);

    assert!(poll(pin!(timers.until(4)).as_mut()).is_ready());
    assert!(poll(pin!(timers.after(0)).as_mut()).is_ready());
}

#[test]
fn drop_stops_wake() {
    let timers = Timers::new();
    {
        let mut sleep = pin!(timers.after(2));
        assert!(poll(sleep.as_mut()).is_pending());
    }
    assert_eq!(timers.advance(2), 0);
}

#[test]
fn timeout_gives_up() {
    let timers = Timers::new();
    let mut wedged = pin!(timers.timeout(2, pending::<u32>()));
    assert!(step(wedged.as_mut()).is_pending());

    assert_eq!(timers.advance(2), 1);

    assert_eq!(step(wedged.as_mut()), Poll::Ready(None));
}

#[test]
fn timeout_keeps_value() {
    let timers = Timers::new();
    let mut answered = pin!(timers.timeout(2, ready(7)));

    assert_eq!(step(answered.as_mut()), Poll::Ready(Some(7)));
}

fn poll(sleep: Pin<&mut Sleep>) -> Poll<()> {
    sleep.poll(&mut Context::from_waker(Waker::noop()))
}

fn step<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
