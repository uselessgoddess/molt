//! Running one future where there is no executor to hand it to.
//!
//! A driver and the single task above it are a loop, not a scheduler: nothing
//! else is runnable while the task waits, so the waker is a noop and the poll
//! comes back around as soon as `serve` has had its turn. Cells that have an
//! executor use one; these are for the places below it.

use core::future::poll_fn;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

/// Polls `future` to completion, running `serve` between polls.
///
/// `serve` must make progress — a driver that answers nothing spins here until
/// the machine is stopped, exactly as the caller asked for.
pub fn drive<F: Future>(future: F, mut serve: impl FnMut()) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        serve();
    }
}

/// Polls `future` at most `rounds` times, returning `None` if it wants more.
///
/// The budget is what makes a watchdog possible from outside the cell: a task
/// that will never finish — waiting on a service that stopped answering, or
/// looping on work that cannot complete — is dropped here rather than spun on,
/// and its supervisor is the one that decides what to do about it.
pub fn drive_bounded<F: Future>(
    future: F,
    rounds: usize,
    mut serve: impl FnMut(),
) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..rounds {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Some(output);
        }
        serve();
    }
    None
}

/// Gives the driver below a turn, asking to be polled again after it.
pub fn defer() -> impl Future<Output = ()> {
    let mut polled = false;
    poll_fn(move |context| {
        if polled {
            return Poll::Ready(());
        }
        polled = true;
        context.waker().wake_by_ref();
        Poll::Pending
    })
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{defer, drive, drive_bounded};

    #[test]
    fn driven_future_sees_every_serve() {
        let mut served = 0;
        let output = drive(
            async {
                defer().await;
                defer().await;
                7
            },
            || served += 1,
        );

        assert_eq!((output, served), (7, 2));
    }

    #[test]
    fn budget_drops_what_never_finishes() {
        let output = drive_bounded(
            async {
                loop {
                    defer().await;
                }
            },
            3,
            || {},
        );

        assert_eq!(output, None::<()>);
    }
}
